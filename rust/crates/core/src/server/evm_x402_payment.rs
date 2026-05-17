//! EVM x402 server middleware.
//!
//! Mirrors the Solana x402 middleware shape but delegates verify+settle to
//! an external facilitator (set via `operator.facilitator_url`). The wire
//! format on the HTTP path is identical to Solana x402:
//! - Challenge header: `PAYMENT-REQUIRED` (base64 JSON envelope)
//! - Settlement header: `PAYMENT-SIGNATURE` or legacy `X-PAYMENT`
//!
//! The differences sit underneath:
//! - The 402 envelope advertises EVM-flavored `PaymentRequirements`
//!   (CAIP-2 network like `eip155:8453`, ERC-20 contract as `currency`, EIP-712
//!   `{name,version}` hint in `extra`).
//! - Verification and on-chain settlement happen via the facilitator's
//!   `/verify` and `/settle` HTTP endpoints — the gateway never holds an
//!   EVM key and never pays gas itself.
//! - Phase 11 hardening:
//!   1. an in-flight `(chain_id, from, nonce)` lock guards the window
//!      between `facilitator.settle` and the on-chain mining;
//!   2. before contacting the facilitator the gateway calls the EIP-3009
//!      `authorizationState(from, nonce)` view on the USDC contract — this
//!      is the authoritative source of truth for already-mined replays;
//!   3. after the facilitator says "settled", the gateway reads the
//!      on-chain receipt via `operator.rpc_url`, decodes the ERC-20
//!      `Transfer` log, and forwards the upstream call only if the receipt
//!      confirms the expected recipient + amount.
//!
//! Gated behind the `evm` feature so a Solana-only build pulls none of the
//! EVM stack.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::json;
use solana_mpp::PAYMENT_RECEIPT_HEADER;
use solana_x402::{PAYMENT_REQUIRED_HEADER, PAYMENT_SIGNATURE_HEADER, X402_V1_PAYMENT_HEADER};

use crate::PaymentState;
use crate::accounts::is_evm_network_family;
use crate::chain::ChainFamily;
use crate::client::balance::{evm_stablecoin_address, evm_stablecoin_decimals};
use crate::client::evm_token_meta::{self, EvmTokenMeta};
use crate::server::in_flight::{InFlight, NonceKey};
use crate::server::metering::{self, RequestProperties};
use crate::server::telemetry;
use crate::server::x402_facilitator::FacilitatorClient;

/// Typed representation of an EVM x402 `PaymentRequirements` entry (Phase 13-2).
///
/// Using a typed struct instead of `serde_json::json!` prevents field-name
/// drift when `x402-chain-eip155` evolves and makes camelCase serialization
/// explicit via `#[serde(rename_all = "camelCase")]`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EvmPaymentRequirements {
    scheme: &'static str,
    network: String,
    asset: String,
    pay_to: String,
    amount: String,
    currency: String,
    decimals: u32,
    resource: String,
    description: String,
    max_amount_required: String,
    max_timeout_seconds: u32,
    extra: EvmExtra,
}

#[derive(serde::Serialize)]
struct EvmExtra {
    name: String,
    version: String,
}

pub async fn evm_x402_payment_middleware<S: PaymentState>(
    axum::extract::State(state): axum::extract::State<S>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().trim_start_matches('/').to_string();

    if path.starts_with("__402/") {
        return next.run(req).await;
    }

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let subdomain = host.split('.').next().unwrap_or("");

    let apis = state.apis();
    let api = match apis.iter().find(|a| a.subdomain == subdomain) {
        Some(api) => api,
        None if apis.len() == 1 => &apis[0],
        None => return next.run(req).await,
    };

    // The EVM middleware is mounted only when `is_evm_x402_spec(...)` accepted
    // the YAML, so reaching any of the next four failure branches means the
    // config drifted out from under the dispatcher. Fail closed — a payment
    // middleware that silently passes free traffic on misconfig is a
    // monetization-bypass hazard.
    let operator = match api.operator.as_ref() {
        Some(op) => op,
        None => {
            return internal_error(
                "EVM x402 middleware mounted but the spec has no `operator` block",
            );
        }
    };

    let network_slug = match operator.network.as_deref() {
        Some(n) if is_evm_network_family(n) => n,
        Some(other) => {
            return internal_error(&format!(
                "EVM x402 middleware mounted but `operator.network` is `{other}` (not an EVM slug)"
            ));
        }
        None => {
            return internal_error("EVM x402 middleware mounted but `operator.network` is unset");
        }
    };
    let recipient = match operator.recipient.as_deref() {
        Some(r) => r,
        None => {
            return internal_error("EVM x402 server has no operator.recipient configured");
        }
    };

    // Phase 11-1 dependency: receipt verification needs an EVM RPC URL.
    // `evm_x402_start::run` rejects boot when `operator.rpc_url` is missing,
    // so reaching here without a URL means a third-party caller mounted the
    // middleware without going through the boot guard.
    let rpc_url = match operator.rpc_url.as_deref() {
        Some(u) if !u.is_empty() => u,
        _ => {
            return internal_error(
                "EVM x402 server requires operator.rpc_url for on-chain receipt verification",
            );
        }
    };

    let facilitator = match state.facilitator() {
        Some(f) => f,
        None => {
            return internal_error(
                "EVM x402 server requires operator.facilitator_url to be configured",
            );
        }
    };

    let in_flight = match state.evm_in_flight() {
        Some(n) => n,
        None => {
            return internal_error(
                "EVM x402 server is missing the in-flight nonce lock (gateway boot bug)",
            );
        }
    };

    let match_method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };

    let endpoint = metering::find_endpoint(api, match_method, &path);
    let metering_config = endpoint.and_then(|ep| ep.metering.as_ref());

    if metering_config.is_none() {
        if api.routing.is_respond() && metering::find_endpoint_by_path(api, &path).is_some() {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"error":"not_found","message":"method not allowed"}"#,
                ))
                .expect("static response with valid status, header, and body never fails to build");
        }
        return next.run(req).await;
    }

    let meter = metering_config.unwrap();
    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);
    // Phase 11-4: surface metering misconfig instead of silently emitting
    // 0.01 USD envelopes when `resolve_price` returns None.
    let amount_usd = match resolve_amount_usd(meter, &props, variant_hint.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            return internal_error(&format!("price_resolution_failed: {e}"));
        }
    };

    // Phase 13-4: advertise all configured currencies in the accepts array.
    // build_evm_requirements is now async (fetches decimals + EIP-712 domain
    // on-chain on first call; subsequent calls read from cache).
    let currency_symbols = pick_currency_symbols(operator);
    let description = endpoint.and_then(|ep| ep.description.as_deref());
    let mut accepts: Vec<serde_json::Value> = Vec::with_capacity(currency_symbols.len());
    for sym in &currency_symbols {
        match build_evm_requirements(
            rpc_url,
            network_slug,
            recipient,
            sym,
            amount_usd,
            &uri,
            description,
        )
        .await
        {
            Ok(req) => accepts.push(req),
            Err(e) => {
                tracing::warn!(symbol = %sym, error = %e, "Skipping currency symbol in accepts")
            }
        }
    }
    if accepts.is_empty() {
        return internal_error("No valid currencies configured for this EVM network");
    }
    let primary_requirements = accepts[0].clone();
    let primary_symbol = &currency_symbols[0];

    let payment_header = headers
        .get(PAYMENT_SIGNATURE_HEADER)
        .or_else(|| headers.get(X402_V1_PAYMENT_HEADER))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    match payment_header {
        None => challenge_response(
            &accepts,
            &method,
            &path,
            subdomain,
            primary_symbol,
            amount_usd,
        ),
        Some(header) => {
            handle_payment(
                facilitator,
                rpc_url,
                in_flight,
                header,
                primary_requirements,
                subdomain,
                &path,
                req,
                next,
            )
            .await
        }
    }
}

fn challenge_response(
    accepts: &[serde_json::Value],
    method: &Method,
    path: &str,
    subdomain: &str,
    currency_symbol: &str,
    amount_usd: f64,
) -> Response {
    let envelope = json!({
        "x402Version": 2,
        "accepts": accepts,
        "resource": null,
    });
    let header_value = match serde_json::to_string(&envelope) {
        Ok(json) => base64::engine::general_purpose::STANDARD.encode(json.as_bytes()),
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize EVM x402 envelope");
            return internal_error("internal_error");
        }
    };

    telemetry::record_402_challenge_sent(
        "x402_evm",
        subdomain,
        path,
        method.as_str(),
        Some(amount_usd),
        currency_symbol,
        1,
    );

    let body = json!({
        "error": "payment_required",
        "message": "This endpoint requires payment.",
        "endpoint": { "method": method.as_str(), "path": path },
        "payment": {
            "protocol": "x402",
            "family": "evm",
            "envelope": envelope,
        },
    });
    let mut response = (StatusCode::PAYMENT_REQUIRED, axum::Json(body)).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&header_value) {
        response
            .headers_mut()
            .insert(PAYMENT_REQUIRED_HEADER, value);
    }
    response
}

#[tracing::instrument(
    name = "evm_x402_payment",
    skip(facilitator, in_flight, header, requirements, req, next),
    fields(subdomain = %subdomain, path = %path)
)]
#[allow(clippy::too_many_arguments)]
async fn handle_payment(
    facilitator: &FacilitatorClient,
    rpc_url: &str,
    in_flight: &InFlight,
    header: String,
    requirements: serde_json::Value,
    subdomain: &str,
    path: &str,
    req: Request<Body>,
    next: Next,
) -> Response {
    let payment_payload = match decode_payment_payload(&header) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Malformed EVM x402 payment header");
            return verification_failed_response(&e);
        }
    };

    // Phase 11-2a: extract the (chain_id, from, nonce) key and pull the
    // expected receipt fields up front. We need both for the in-flight
    // guard and for the on-chain `authorizationState` check below.
    let nonce_key = match NonceKey::from_envelope(&payment_payload, &requirements) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "EVM x402 envelope missing nonce metadata");
            return verification_failed_response(&format!("malformed payment envelope: {e}"));
        }
    };
    let expected = match extract_receipt_expectations(&requirements) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "EVM requirements missing fields needed for receipt check");
            return internal_error(&format!("receipt_check_setup_failed: {e}"));
        }
    };

    // Cross-check that the EIP-3009 authorization the payer signed targets
    // the same token contract the requirements describe. Without this, a
    // payer could submit a signature against a *different* asset; the
    // facilitator would still reject it, but the error would surface as a
    // misleading "nonce_already_used_on_chain" because
    // `authorizationState(expected.asset, …)` returns `false` for an
    // unrelated contract's nonce-space.
    let authorization_to = payment_payload
        .pointer("/payload/authorization/to")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !addresses_equal(authorization_to, &expected.asset) {
        let msg = format!(
            "authorization.to `{authorization_to}` does not match requirements.asset `{}`",
            expected.asset
        );
        tracing::warn!(error = %msg, "EVM x402 envelope targets wrong token contract");
        return verification_failed_response(&msg);
    }

    // Local `validBefore` sanity check. Saves a facilitator round trip when
    // the authorization is already expired and adds defense-in-depth against
    // a facilitator with clock skew. We accept either decimal or 0x-hex
    // strings (EIP-3009 encodes uint256 timestamps both ways in the wild)
    // and only reject when we can definitively prove the deadline has
    // passed — a missing/unparseable field is left for the facilitator.
    if let Some(valid_before_str) = payment_payload
        .pointer("/payload/authorization/validBefore")
        .and_then(|v| v.as_str())
    {
        let parsed = if let Some(hex) = valid_before_str.strip_prefix("0x") {
            u64::from_str_radix(hex, 16).ok()
        } else {
            valid_before_str.parse::<u64>().ok()
        };
        if let Some(valid_before) = parsed {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if valid_before <= now {
                let msg = format!(
                    "authorization.validBefore `{valid_before}` is in the past (now={now})"
                );
                telemetry::record_settlement_error(
                    "x402_evm",
                    subdomain,
                    path,
                    "authorization_expired",
                    false,
                );
                return verification_failed_response(&msg);
            }
        }
    }

    // Phase 11-2b: take the in-flight slot. This is the only thing that
    // closes the window between `facilitator.settle` kicking off and the
    // on-chain authorization-state flipping to `true` — both the gateway-
    // local `authorizationState` view and Phase 11-1's receipt check are
    // blind to a duplicate that arrives *during* that window. The guard
    // releases automatically on every exit path (including the unwind).
    // The leading `_` keeps clippy quiet about the unused binding while
    // the explicit name documents the load-bearing RAII lifetime — drop
    // it and the duplicate window re-opens.
    let _in_flight_guard = match in_flight.try_acquire(nonce_key) {
        Some(g) => g,
        None => {
            telemetry::record_settlement_error(
                "x402_evm",
                subdomain,
                path,
                "in_flight_duplicate",
                false,
            );
            return verification_failed_response(
                "payment authorization is already being processed",
            );
        }
    };

    // Phase 11-2c: the authoritative replay check. EIP-3009 maintains a
    // permanent `_authorizationStates[from][nonce]` flag on the token
    // contract; if it's already `true`, the second `transferWithAuthorization`
    // call would revert. We pre-check it so we don't have to pay a
    // facilitator round trip (and a potential receipt poll) for a no-op.
    match check_authorization_state(rpc_url, &expected.asset, nonce_key.from, nonce_key.nonce).await
    {
        Ok(true) => {
            telemetry::record_settlement_error(
                "x402_evm",
                subdomain,
                path,
                "nonce_already_used_on_chain",
                false,
            );
            return verification_failed_response(
                "payment authorization already used on-chain (replay)",
            );
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "authorizationState eth_call failed");
            return internal_error(&format!("authorization_state_check_failed: {e}"));
        }
    }

    match facilitator.verify(&payment_payload, &requirements).await {
        Ok(resp) if !resp.is_valid => {
            let reason = resp.invalid_reason.unwrap_or_else(|| "invalid".to_string());
            telemetry::record_settlement_error("x402_evm", subdomain, path, &reason, false);
            return verification_failed_response(&reason);
        }
        Err(e) => {
            let retryable = e.is_retryable();
            let msg = e.to_string();
            telemetry::record_settlement_error("x402_evm", subdomain, path, &msg, retryable);
            return verification_failed_response(&msg);
        }
        Ok(_) => {}
    }

    let settle = match facilitator.settle(&payment_payload, &requirements).await {
        Ok(r) => r,
        Err(e) => {
            let retryable = e.is_retryable();
            let msg = e.to_string();
            telemetry::record_settlement_error("x402_evm", subdomain, path, &msg, retryable);
            return verification_failed_response(&msg);
        }
    };
    if !settle.success {
        let reason = settle
            .error_reason
            .unwrap_or_else(|| "settlement failed".to_string());
        telemetry::record_settlement_error("x402_evm", subdomain, path, &reason, false);
        return verification_failed_response(&reason);
    }

    let tx_hash = match settle.transaction.as_deref() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => {
            telemetry::record_settlement_error(
                "x402_evm",
                subdomain,
                path,
                "missing_tx_hash",
                false,
            );
            return verification_failed_response(
                "facilitator reported settlement success but returned no transaction hash",
            );
        }
    };

    // Phase 11-1: independent on-chain check that the facilitator actually
    // moved the expected funds. Without this, a misbehaving facilitator can
    // pair `success:true` with an underpaid (or non-existent) transfer.
    // `expected` was already extracted at the top of `handle_payment` so the
    // authorization-state pre-check and the receipt check use the same
    // requirements view.
    if let Err(e) = verify_onchain_receipt(
        rpc_url,
        &tx_hash,
        &expected.recipient,
        &expected.asset,
        expected.min_amount_raw,
    )
    .await
    {
        telemetry::record_settlement_error("x402_evm", subdomain, path, &e, false);
        return verification_failed_response(&e);
    }

    tracing::info!(
        subdomain = %subdomain,
        path = %path,
        transaction = %tx_hash,
        "EVM x402 payment settled + receipt verified — forwarding"
    );
    telemetry::record_payment_collected("x402_evm", subdomain, path, None, &tx_hash);

    let mut response = next.run(req).await;
    let status = response.status();
    telemetry::record_paid_request_completed("x402_evm", subdomain, path, status, None);

    // Phase 11-3: expose the tx hash to the client so the CLI receipt
    // collector (already wired for Solana via the same header constant)
    // can show it.
    if let Ok(value) = axum::http::HeaderValue::from_str(&tx_hash) {
        response.headers_mut().insert(PAYMENT_RECEIPT_HEADER, value);
    }
    response
}

fn decode_payment_payload(header: &str) -> Result<serde_json::Value, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(header.trim())
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|e| format!("JSON decode failed: {e}"))
}

fn pick_currency_symbols(operator: &pay_types::metering::OperatorConfig) -> Vec<String> {
    let list = operator.currencies.get("usd").cloned().unwrap_or_default();
    if list.is_empty() {
        vec!["USDC".to_string()]
    } else {
        list
    }
}

/// Build a single `PaymentRequirements` JSON object for one currency symbol.
///
/// Phase 13-1+3: now async so it can call `fetch_token_meta` which issues
/// `decimals()` and `eip712Domain()` view calls on first invocation and reads
/// from an in-process cache on subsequent ones.  If the RPC call fails the
/// function falls back to static values (symbol-based decimals +
/// `static_fallback_domain`) so the gateway can still respond on RPC hiccups.
async fn build_evm_requirements(
    rpc_url: &str,
    network_slug: &str,
    recipient: &str,
    currency_symbol: &str,
    amount_usd: f64,
    uri: &axum::http::Uri,
    description: Option<&str>,
) -> Result<serde_json::Value, String> {
    let chain_id = match ChainFamily::from_network_slug(network_slug) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => return Err(format!("Network `{network_slug}` is not an EVM network")),
    };
    let asset = evm_stablecoin_address(network_slug, currency_symbol).ok_or_else(|| {
        format!("No known ERC-20 deployment for {currency_symbol} on {network_slug}")
    })?;
    // Validate price before fetching metadata — avoids a pointless RPC call
    // for obviously invalid inputs (NaN, negative, overflow).
    if !amount_usd.is_finite() || amount_usd < 0.0 {
        return Err(format!(
            "Invalid EVM x402 price `{amount_usd}` (must be a finite non-negative number)"
        ));
    }

    // Phase 13-1+3: fetch decimals and EIP-712 domain from the token contract.
    // Falls back to static table when the RPC URL is unavailable or the token
    // does not implement EIP-5267 — the fallback values mirror what
    // `x402-chain-eip155` ships for USDC so the facilitator accepts the envelope.
    let token: alloy::primitives::Address = asset
        .parse()
        .map_err(|e| format!("token address `{asset}` parse failed: {e}"))?;
    let meta: EvmTokenMeta =
        match evm_token_meta::fetch_token_meta(rpc_url, chain_id, token, Some(currency_symbol))
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    network = %network_slug,
                    currency = %currency_symbol,
                    "fetch_token_meta failed — using static fallback for envelope"
                );
                EvmTokenMeta {
                    decimals: evm_stablecoin_decimals(currency_symbol).ok_or_else(|| {
                        format!(
                            "Unknown stablecoin `{currency_symbol}` and on-chain decimals() \
                         call failed — cannot build envelope"
                        )
                    })?,
                    eip712_domain: evm_token_meta::static_fallback_domain(chain_id),
                }
            }
        };

    let decimals = meta.decimals as u32;
    // Scale via fixed-precision string formatting rather than `amount_usd * 10^decimals`.
    // f64's 53-bit mantissa cannot represent the base-unit count exactly for 18-decimal
    // tokens (e.g. DAI at $1000 → 10^21 base units) and loses cents for USDC values like
    // `0.100001` → 100000.999… which `as u128` would truncate to 100000, producing
    // an underpaid envelope that the facilitator rejects.
    let formatted = format!("{:.*}", decimals as usize, amount_usd);
    let combined: String = formatted.chars().filter(|c| *c != '.').collect();
    let raw_amount: u128 = combined.parse().map_err(|_| {
        format!("EVM x402 price `{amount_usd}` exceeds the u128 base-unit ceiling")
    })?;

    // Phase 13-2: typed struct serialization instead of ad-hoc json! macro.
    let req = EvmPaymentRequirements {
        scheme: "exact",
        network: format!("eip155:{chain_id}"),
        asset: asset.to_string(),
        pay_to: recipient.to_string(),
        amount: raw_amount.to_string(),
        currency: asset.to_string(),
        decimals,
        resource: uri.to_string(),
        description: description.unwrap_or("").to_string(),
        max_amount_required: raw_amount.to_string(),
        max_timeout_seconds: 300,
        extra: EvmExtra {
            name: meta.eip712_domain.name,
            version: meta.eip712_domain.version,
        },
    };
    serde_json::to_value(&req).map_err(|e| format!("requirements serialize failed: {e}"))
}

/// Subset of `requirements` we re-check against the on-chain receipt.
#[derive(Debug)]
struct ReceiptExpectations {
    recipient: String,
    asset: String,
    min_amount_raw: u128,
}

fn extract_receipt_expectations(
    requirements: &serde_json::Value,
) -> Result<ReceiptExpectations, String> {
    let recipient = requirements
        .get("payTo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "requirements.payTo missing".to_string())?
        .to_string();
    let asset = requirements
        .get("asset")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "requirements.asset missing".to_string())?
        .to_string();
    let amount_str = requirements
        .get("amount")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "requirements.amount missing".to_string())?;
    let min_amount_raw: u128 = amount_str
        .parse()
        .map_err(|e| format!("requirements.amount `{amount_str}` is not a u128: {e}"))?;
    Ok(ReceiptExpectations {
        recipient,
        asset,
        min_amount_raw,
    })
}

/// Read the transaction receipt off-chain via JSON-RPC and confirm the
/// expected ERC-20 `Transfer(_, recipient, value)` log is present with
/// `value >= expected_min_amount_raw`. Returns `Ok(())` on success; any
/// other outcome (missing tx, reverted, wrong recipient, underpaid) maps to
/// a verification failure with a descriptive message.
async fn verify_onchain_receipt(
    rpc_url: &str,
    tx_hash: &str,
    expected_recipient: &str,
    expected_asset: &str,
    expected_min_amount_raw: u128,
) -> Result<(), String> {
    use alloy::primitives::{Address, B256, U256, keccak256};
    use alloy::providers::{Provider, ProviderBuilder};
    use std::str::FromStr;

    let parsed_url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| format!("invalid operator.rpc_url `{rpc_url}`: {e}"))?;
    let provider = ProviderBuilder::new().connect_http(parsed_url);

    let tx_hash_clean = tx_hash.trim();
    let tx_hash_b256 = B256::from_str(tx_hash_clean)
        .map_err(|e| format!("settle returned invalid tx hash `{tx_hash_clean}`: {e}"))?;

    let receipt = provider
        .get_transaction_receipt(tx_hash_b256)
        .await
        .map_err(|e| format!("eth_getTransactionReceipt failed: {e}"))?
        .ok_or_else(|| {
            format!("transaction {tx_hash_clean} not yet visible — refuse to forward")
        })?;

    if !receipt.status() {
        return Err(format!(
            "transaction {tx_hash_clean} reverted on-chain; refusing to grant access"
        ));
    }

    let asset_addr = Address::from_str(expected_asset)
        .map_err(|e| format!("requirements.asset `{expected_asset}` invalid: {e}"))?;
    let recipient_addr = Address::from_str(expected_recipient)
        .map_err(|e| format!("requirements.payTo `{expected_recipient}` invalid: {e}"))?;

    let transfer_topic = keccak256("Transfer(address,address,uint256)".as_bytes());
    let expected_to_topic = address_as_topic(recipient_addr);
    let expected_min = U256::from(expected_min_amount_raw);

    let mut best_value: Option<U256> = None;
    for log in receipt.inner.logs() {
        if log.address() != asset_addr {
            continue;
        }
        let topics = log.topics();
        let Some(first) = topics.first() else {
            continue;
        };
        if first.as_slice() != transfer_topic.as_slice() {
            continue;
        }
        let Some(to_topic) = topics.get(2) else {
            continue;
        };
        if to_topic.as_slice() != expected_to_topic.as_slice() {
            continue;
        }
        let data = log.data().data.as_ref();
        if data.len() != 32 {
            continue;
        }
        let value = U256::from_be_slice(data);
        best_value = Some(match best_value {
            Some(prev) if prev > value => prev,
            _ => value,
        });
    }

    let value = best_value.ok_or_else(|| {
        format!(
            "tx {tx_hash_clean} has no ERC-20 Transfer({expected_asset} → {expected_recipient}) log"
        )
    })?;
    if value < expected_min {
        return Err(format!(
            "tx {tx_hash_clean} underpaid: on-chain value {value} < expected {expected_min}"
        ));
    }
    Ok(())
}

/// Case-insensitive 0x-hex equality for two EVM addresses. Both inputs go
/// through `alloy::primitives::Address` parsing so the comparison ignores
/// EIP-55 checksum case and tolerates a missing `0x` prefix.
fn addresses_equal(a: &str, b: &str) -> bool {
    use std::str::FromStr;
    match (
        alloy::primitives::Address::from_str(a),
        alloy::primitives::Address::from_str(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn address_as_topic(addr: alloy::primitives::Address) -> alloy::primitives::B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(addr.as_slice());
    alloy::primitives::B256::from(bytes)
}

// EIP-3009 standardizes a `authorizationState(authorizer, nonce) -> bool`
// view on every token that supports `transferWithAuthorization`. We call it
// before contacting the facilitator so a replay of an already-mined
// authorization is rejected without doing any off-gateway work.
alloy::sol! {
    #[sol(rpc)]
    interface IEip3009 {
        function authorizationState(address authorizer, bytes32 nonce)
            external view returns (bool);
    }
}

async fn check_authorization_state(
    rpc_url: &str,
    asset_hex: &str,
    from: [u8; 20],
    nonce: [u8; 32],
) -> Result<bool, String> {
    use alloy::primitives::{Address, B256};
    use alloy::providers::ProviderBuilder;
    use std::str::FromStr;

    let parsed_url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| format!("invalid operator.rpc_url `{rpc_url}`: {e}"))?;
    let provider = ProviderBuilder::new().connect_http(parsed_url);

    let asset_addr = Address::from_str(asset_hex)
        .map_err(|e| format!("requirements.asset `{asset_hex}` invalid: {e}"))?;
    let from_addr = Address::from(from);
    let nonce_b = B256::from(nonce);

    IEip3009::new(asset_addr, &provider)
        .authorizationState(from_addr, nonce_b)
        .call()
        .await
        .map_err(|e| format!("authorizationState eth_call failed: {e}"))
}

/// Resolve the per-endpoint USD price. Phase 11-4: returning `Err` (instead
/// of silently substituting 0.01 USD) makes a metering misconfig visible as
/// a 500 rather than turning every request into a 1¢ charge.
fn resolve_amount_usd(
    meter: &pay_types::metering::Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
) -> Result<f64, String> {
    let price = metering::resolve_price(meter, props, variant_hint, None)
        .ok_or_else(|| "metering returned no price for this request".to_string())?;
    let dim = price
        .dimensions
        .first()
        .ok_or_else(|| "metering price has no dimensions".to_string())?;
    let scale = dim.scale.max(1) as f64;
    Ok(dim.price_usd / scale)
}

fn extract_request_properties(headers: &HeaderMap, _path: &str) -> RequestProperties {
    let body_size = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    RequestProperties {
        body_size,
        ..Default::default()
    }
}

fn extract_variant_hint(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if (*part == "models" || *part == "voices")
            && let Some(next) = parts.get(i + 1)
        {
            return Some(next.split(':').next().unwrap_or(next).to_string());
        }
    }
    None
}

fn internal_error(message: &str) -> Response {
    tracing::error!(message, "evm_x402 middleware aborted");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({
            "error": "internal_error",
            "message": message,
        })),
    )
        .into_response()
}

fn verification_failed_response(message: &str) -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        axum::Json(json!({
            "error": "verification_failed",
            "message": message,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // rpc_url="" causes fetch_token_meta to fail URL parse → falls back to
    // static decimals + static_fallback_domain, which is what these tests assert.

    #[tokio::test]
    async fn build_evm_requirements_for_sepolia_usdc() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let req = build_evm_requirements(
            "",
            "sepolia",
            "0xabc0000000000000000000000000000000000001",
            "USDC",
            0.10,
            &uri,
            Some("desc"),
        )
        .await
        .unwrap();
        assert_eq!(req["scheme"], "exact");
        assert_eq!(req["network"], "eip155:11155111");
        assert_eq!(req["asset"], "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
        assert_eq!(req["decimals"], 6);
        assert_eq!(req["amount"], "100000");
        assert_eq!(req["extra"]["name"], "USDC");
        assert_eq!(req["extra"]["version"], "2");
    }

    #[tokio::test]
    async fn build_evm_requirements_for_base_uses_long_name() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let req = build_evm_requirements(
            "",
            "base",
            "0xabc0000000000000000000000000000000000001",
            "USDC",
            1.0,
            &uri,
            None,
        )
        .await
        .unwrap();
        assert_eq!(req["network"], "eip155:8453");
        assert_eq!(req["extra"]["name"], "USD Coin");
        assert_eq!(req["amount"], "1000000");
    }

    #[tokio::test]
    async fn build_evm_requirements_rejects_non_evm() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements("", "mainnet", "ABC", "USDC", 1.0, &uri, None)
            .await
            .unwrap_err();
        assert!(err.contains("not an EVM network"));
    }

    #[tokio::test]
    async fn build_evm_requirements_rejects_unknown_currency() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements("", "sepolia", "0xabc", "DOGE", 1.0, &uri, None)
            .await
            .unwrap_err();
        assert!(err.contains("No known ERC-20 deployment"));
    }

    #[tokio::test]
    async fn build_evm_requirements_rejects_nan_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements("", "sepolia", "0xabc", "USDC", f64::NAN, &uri, None)
            .await
            .unwrap_err();
        assert!(err.contains("finite"));
    }

    #[tokio::test]
    async fn build_evm_requirements_rejects_negative_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements("", "sepolia", "0xabc", "USDC", -1.0, &uri, None)
            .await
            .unwrap_err();
        assert!(err.contains("non-negative"));
    }

    #[tokio::test]
    async fn build_evm_requirements_rejects_oversize_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        // 1e35 USD × 10^6 base units = 1e41 > u128::MAX (~3.4e38).
        let err = build_evm_requirements("", "sepolia", "0xabc", "USDC", 1e35, &uri, None)
            .await
            .unwrap_err();
        assert!(err.contains("u128 base-unit ceiling"), "got: {err}");
    }

    #[test]
    fn pick_currency_symbols_returns_configured_list() {
        let op: pay_types::metering::OperatorConfig = serde_json::from_value(serde_json::json!({
            "currencies": { "usd": ["USDC", "USDT"] }
        }))
        .unwrap();
        let syms = pick_currency_symbols(&op);
        assert_eq!(syms, vec!["USDC", "USDT"]);
    }

    #[test]
    fn pick_currency_symbols_defaults_to_usdc() {
        let op: pay_types::metering::OperatorConfig =
            serde_json::from_value(serde_json::json!({})).unwrap();
        let syms = pick_currency_symbols(&op);
        assert_eq!(syms, vec!["USDC"]);
    }

    #[test]
    fn extract_receipt_expectations_parses_required_fields() {
        let req = serde_json::json!({
            "payTo": "0x2222222222222222222222222222222222222222",
            "asset": "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
            "amount": "100000"
        });
        let e = extract_receipt_expectations(&req).expect("parse");
        assert_eq!(e.recipient, "0x2222222222222222222222222222222222222222");
        assert_eq!(e.asset, "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
        assert_eq!(e.min_amount_raw, 100_000);
    }

    #[test]
    fn extract_receipt_expectations_rejects_missing_amount() {
        let req = serde_json::json!({
            "payTo": "0x22",
            "asset": "0x1c",
        });
        let err = extract_receipt_expectations(&req).unwrap_err();
        assert!(err.contains("amount"));
    }

    #[test]
    fn address_as_topic_left_pads_address() {
        use alloy::primitives::Address;
        let addr = Address::from([0xab; 20]);
        let topic = address_as_topic(addr);
        let bytes = topic.as_slice();
        assert_eq!(&bytes[..12], &[0u8; 12]);
        assert_eq!(&bytes[12..], &[0xab; 20]);
    }

    fn empty_metering() -> pay_types::metering::Metering {
        pay_types::metering::Metering {
            dimensions: Vec::new(),
            variants: Vec::new(),
            sku_tiers: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn resolve_amount_usd_errors_when_metering_yields_no_price() {
        // An empty metering config produces no resolved price, which Phase 11-4
        // now surfaces as an explicit error rather than the old 0.01 USD
        // silent fallback.
        let meter = empty_metering();
        let err = resolve_amount_usd(&meter, &RequestProperties::default(), None).unwrap_err();
        assert!(err.contains("no price"), "got: {err}");
    }
}
