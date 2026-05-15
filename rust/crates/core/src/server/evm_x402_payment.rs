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
//!
//! Gated behind the `evm` feature so a Solana-only build pulls none of the
//! EVM stack.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::json;
use solana_x402::{PAYMENT_REQUIRED_HEADER, PAYMENT_SIGNATURE_HEADER, X402_V1_PAYMENT_HEADER};

use crate::PaymentState;
use crate::accounts::is_evm_network_family;
use crate::chain::ChainFamily;
use crate::client::balance::evm_stablecoin_address;
use crate::server::metering::{self, RequestProperties};
use crate::server::telemetry;
use crate::server::x402_facilitator::FacilitatorClient;

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
    // the YAML, so reaching any of the next three failure branches means the
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
            return internal_error(
                "EVM x402 middleware mounted but `operator.network` is unset",
            );
        }
    };
    let recipient = match operator.recipient.as_deref() {
        Some(r) => r,
        None => {
            return internal_error("EVM x402 server has no operator.recipient configured");
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

    let match_method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };

    let endpoint = metering::find_endpoint(api, match_method, &path);
    let metering_config = endpoint.and_then(|ep| ep.metering.as_ref());

    if metering_config.is_none() {
        if api.routing.is_respond()
            && metering::find_endpoint_by_path(api, &path).is_some()
        {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"error":"not_found","message":"method not allowed"}"#,
                ))
                .unwrap();
        }
        return next.run(req).await;
    }

    let meter = metering_config.unwrap();
    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);
    let amount_usd = resolve_amount_usd(meter, &props, variant_hint.as_deref());

    let currency_symbol = pick_currency_symbol(operator);
    let requirements = match build_evm_requirements(
        network_slug,
        recipient,
        &currency_symbol,
        amount_usd,
        &uri,
        endpoint.and_then(|ep| ep.description.as_deref()),
    ) {
        Ok(r) => r,
        Err(e) => return internal_error(&e),
    };

    let payment_header = headers
        .get(PAYMENT_SIGNATURE_HEADER)
        .or_else(|| headers.get(X402_V1_PAYMENT_HEADER))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    match payment_header {
        None => challenge_response(
            &requirements,
            &method,
            &path,
            subdomain,
            &currency_symbol,
            amount_usd,
        ),
        Some(header) => {
            handle_payment(
                facilitator,
                header,
                requirements,
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
    requirements: &serde_json::Value,
    method: &Method,
    path: &str,
    subdomain: &str,
    currency_symbol: &str,
    amount_usd: f64,
) -> Response {
    let envelope = json!({
        "x402Version": 2,
        "accepts": [requirements],
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
    skip(facilitator, header, requirements, req, next),
    fields(subdomain = %subdomain, path = %path)
)]
async fn handle_payment(
    facilitator: &FacilitatorClient,
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

    match facilitator.verify(&payment_payload, &requirements).await {
        Ok(resp) if !resp.is_valid => {
            let reason = resp.invalid_reason.unwrap_or_else(|| "invalid".to_string());
            telemetry::record_settlement_error("x402_evm", subdomain, path, &reason, false);
            return verification_failed_response(&reason);
        }
        Err(e) => {
            let msg = e.to_string();
            telemetry::record_settlement_error("x402_evm", subdomain, path, &msg, true);
            return verification_failed_response(&msg);
        }
        Ok(_) => {}
    }

    let settle = match facilitator.settle(&payment_payload, &requirements).await {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            telemetry::record_settlement_error("x402_evm", subdomain, path, &msg, true);
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

    let tx_hash = settle.transaction.unwrap_or_default();
    tracing::info!(
        subdomain = %subdomain,
        path = %path,
        transaction = %tx_hash,
        "EVM x402 payment settled via facilitator — forwarding"
    );
    telemetry::record_payment_collected("x402_evm", subdomain, path, None, &tx_hash);

    let response = next.run(req).await;
    let status = response.status();
    telemetry::record_paid_request_completed("x402_evm", subdomain, path, status, None);
    response
}

fn decode_payment_payload(header: &str) -> Result<serde_json::Value, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(header.trim())
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|e| format!("JSON decode failed: {e}"))
}

fn pick_currency_symbol(operator: &pay_types::metering::OperatorConfig) -> String {
    operator
        .currencies
        .get("usd")
        .and_then(|list| list.first().cloned())
        .unwrap_or_else(|| "USDC".to_string())
}

fn build_evm_requirements(
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
    // USDC is 6-decimal on every supported chain. Generalize when we add
    // non-USDC support; for now this matches the registry's pinned tokens.
    let decimals = 6u32;
    // Validate the price before scaling: `as u128` on an f64 saturates
    // (NaN→0, negative→0, oversize→u128::MAX), which silently emits free /
    // outrageously-priced envelopes. Reject those at the source.
    if !amount_usd.is_finite() || amount_usd < 0.0 {
        return Err(format!(
            "Invalid EVM x402 price `{amount_usd}` (must be a finite non-negative number)"
        ));
    }
    let scaled = amount_usd * 10f64.powi(decimals as i32);
    if scaled > (u64::MAX as f64) {
        return Err(format!(
            "EVM x402 price `{amount_usd}` exceeds the u64 base-unit ceiling"
        ));
    }
    let raw_amount = scaled.round() as u128;
    let (token_name, token_version) = usdc_eip712_domain(network_slug);

    Ok(json!({
        "scheme": "exact",
        "network": format!("eip155:{chain_id}"),
        "asset": asset,
        "payTo": recipient,
        "amount": raw_amount.to_string(),
        "currency": asset,
        "decimals": decimals,
        "resource": uri.to_string(),
        "description": description.unwrap_or(""),
        "maxAmountRequired": raw_amount.to_string(),
        "maxTimeoutSeconds": 300,
        "extra": {
            "name": token_name,
            "version": token_version,
        }
    }))
}

/// EIP-712 domain hint per (chain, USDC) deployment. Mirrors the values
/// `x402-chain-eip155` ships under `KnownNetworkEip155 for USDC` so the
/// facilitator's signature check accepts our envelope.
fn usdc_eip712_domain(network_slug: &str) -> (&'static str, &'static str) {
    match network_slug {
        // Ethereum mainnet & most L2s use the long form name.
        "ethereum" | "base" | "optimism" | "arbitrum" => ("USD Coin", "2"),
        // Sepolia/Holesky/Base-Sepolia testnets use the short form.
        "sepolia" | "holesky" | "base-sepolia" => ("USDC", "2"),
        _ => ("USDC", "2"),
    }
}

fn resolve_amount_usd(
    meter: &pay_types::metering::Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
) -> f64 {
    metering::resolve_price(meter, props, variant_hint, None)
        .and_then(|p| p.dimensions.first().cloned())
        .map(|d| d.price_usd / d.scale.max(1) as f64)
        .unwrap_or(0.01)
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

    #[test]
    fn build_evm_requirements_for_sepolia_usdc() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let req = build_evm_requirements(
            "sepolia",
            "0xabc0000000000000000000000000000000000001",
            "USDC",
            0.10,
            &uri,
            Some("desc"),
        )
        .unwrap();
        assert_eq!(req["scheme"], "exact");
        assert_eq!(req["network"], "eip155:11155111");
        assert_eq!(req["asset"], "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
        assert_eq!(req["decimals"], 6);
        assert_eq!(req["amount"], "100000");
        assert_eq!(req["extra"]["name"], "USDC");
        assert_eq!(req["extra"]["version"], "2");
    }

    #[test]
    fn build_evm_requirements_for_base_uses_long_name() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let req = build_evm_requirements(
            "base",
            "0xabc0000000000000000000000000000000000001",
            "USDC",
            1.0,
            &uri,
            None,
        )
        .unwrap();
        assert_eq!(req["network"], "eip155:8453");
        assert_eq!(req["extra"]["name"], "USD Coin");
        assert_eq!(req["amount"], "1000000");
    }

    #[test]
    fn build_evm_requirements_rejects_non_evm() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements(
            "mainnet",
            "ABC",
            "USDC",
            1.0,
            &uri,
            None,
        )
        .unwrap_err();
        assert!(err.contains("not an EVM network"));
    }

    #[test]
    fn build_evm_requirements_rejects_unknown_currency() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements(
            "sepolia",
            "0xabc",
            "DOGE",
            1.0,
            &uri,
            None,
        )
        .unwrap_err();
        assert!(err.contains("No known ERC-20 deployment"));
    }

    #[test]
    fn build_evm_requirements_rejects_nan_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements(
            "sepolia",
            "0xabc",
            "USDC",
            f64::NAN,
            &uri,
            None,
        )
        .unwrap_err();
        assert!(err.contains("finite"));
    }

    #[test]
    fn build_evm_requirements_rejects_negative_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        let err = build_evm_requirements(
            "sepolia",
            "0xabc",
            "USDC",
            -1.0,
            &uri,
            None,
        )
        .unwrap_err();
        assert!(err.contains("non-negative"));
    }

    #[test]
    fn build_evm_requirements_rejects_oversize_price() {
        let uri: axum::http::Uri = "/v1/test".parse().unwrap();
        // 1e30 USD × 10^6 base units > u64::MAX (~1.8e19).
        let err = build_evm_requirements(
            "sepolia",
            "0xabc",
            "USDC",
            1e30,
            &uri,
            None,
        )
        .unwrap_err();
        assert!(err.contains("u64 base-unit ceiling"));
    }
}
