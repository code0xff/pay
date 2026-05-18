//! Endpoint probing — verify that provider endpoints return valid Solana 402 challenges.
//!
//! Used by `pay skills probe` CLI and CI to verify that every listed endpoint
//! actually accepts payment via the expected stablecoins on Solana.
//!
//! Each probe captures the *full* set of payment options advertised by the
//! server: every MPP challenge in the `www-authenticate` header (not just the
//! one Pay would settle on) and every entry in the x402 `accepts` body.
//! Downstream tooling (`pay skills build`) uses that to populate per-endpoint
//! `pricing`, `protocol`, and `supported_usd` fields in the published index.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use pay_types::Stablecoin;
use serde::Serialize;
use serde_json::Value;

use crate::client::fetch::fetch_raw;
use crate::client::runner::{self, RunOutcome};

// ── Currency normalization ───────────────────────────────────────────────────

/// Known Solana mint addresses → symbol mappings.
const MINT_MAP: &[(&str, &str, u8)] = &[
    // (mint, symbol, decimals)
    (pay_types::stablecoin_mints::USDC_MAINNET, "USDC", 6),
    (pay_types::stablecoin_mints::USDC_DEVNET, "USDC", 6),
    (pay_types::stablecoin_mints::USDT_MAINNET, "USDT", 6),
    (pay_types::stablecoin_mints::PYUSD_MAINNET, "PYUSD", 6),
    (pay_types::stablecoin_mints::PYUSD_DEVNET, "PYUSD", 6),
    (pay_types::stablecoin_mints::CASH_MAINNET, "CASH", 6),
    (pay_types::stablecoin_mints::USDG_MAINNET, "USDG", 6),
    ("So11111111111111111111111111111111111111112", "SOL", 9),
];

/// Normalize a currency identifier to its symbol (uppercase).
/// Recognizes known mint addresses and maps them to symbols.
fn normalize_currency(raw: &str) -> String {
    for (mint, symbol, _) in MINT_MAP {
        if raw == *mint {
            return symbol.to_string();
        }
    }
    raw.to_uppercase()
}

/// Decimal scale for a currency symbol. Defaults to 6 (matches USDC/USDT) when
/// unknown — every known x402 stablecoin uses 6 decimals.
fn decimals_for(symbol: &str) -> u8 {
    for (_, sym, decimals) in MINT_MAP {
        if symbol.eq_ignore_ascii_case(sym) {
            return *decimals;
        }
    }
    6
}

fn is_usd_stable(symbol: &str) -> bool {
    Stablecoin::parse_symbol(symbol).is_some()
}

/// Solana CAIP-2 networks all start with `solana:`. Treat the bare slug
/// `"solana"` and `"mainnet-beta"` as Solana too — some servers ship those.
fn is_solana_network(raw: &str) -> bool {
    raw.starts_with("solana:")
        || raw.eq_ignore_ascii_case("solana")
        || raw.eq_ignore_ascii_case("mainnet-beta")
        || raw.eq_ignore_ascii_case("solana-mainnet")
}

/// Convert a base-units string (e.g. `"10000"` of 6-decimal USDC) to USD.
fn amount_to_usd(amount_str: &str, decimals: u8) -> Option<f64> {
    let raw: u128 = amount_str.parse().ok()?;
    let divisor = 10u128.pow(decimals as u32) as f64;
    Some(raw as f64 / divisor)
}

/// Resolve an x402 accept entry to a CAIP-2 network + canonical symbol +
/// decimals — handles both Solana (SPL mint mapping) and EVM (ERC-20
/// address lookup gated on the `evm` Cargo feature). Returns `None` for any
/// network/token combo we can't index (so the caller silently skips it
/// rather than emitting a useless empty offer).
/// Phase 15: choose the "primary" x402 accept entry for `ProbeStatus::Ok`.
///
/// `paid.chain_offers` carries the full multi-chain list independently, so
/// this only picks which entry seeds the legacy flat `network`/`currency`
/// fields. **EVM-first when the `evm` Cargo feature is enabled** — an
/// EVM-built `pay` has explicitly opted into EVM and should highlight EVM
/// options in the index for matching downstream consumers; the Solana-only
/// build naturally falls through to Solana.
fn pick_indexable_x402<'a>(
    candidates: &'a [crate::x402_proto::PaymentRequirements],
) -> Option<&'a crate::x402_proto::PaymentRequirements> {
    {
        if let Some(r) = candidates
            .iter()
            .find(|r| resolve_offer(&r.network, &r.currency).is_some_and(|(caip2, _, _)| caip2.starts_with("eip155:")))
        {
            return Some(r);
        }
    }
    if let Some(r) = candidates
        .iter()
        .find(|r| resolve_offer(&r.network, &r.currency).is_some_and(|(caip2, _, _)| caip2.starts_with("solana:")))
    {
        return Some(r);
    }
    // Last resort: any entry resolve_offer accepts (covers unknown CAIP-2
    // prefixes added later without requiring a probe-side update).
    candidates
        .iter()
        .find(|r| resolve_offer(&r.network, &r.currency).is_some())
}

fn resolve_offer(network: &str, asset: &str) -> Option<(String, String, u8)> {
    if is_solana_network(network) {
        let symbol = normalize_currency(asset);
        if !is_usd_stable(&symbol) {
            return None;
        }
        // Preserve the server's CAIP-2 form if it sent one; otherwise pin
        // to mainnet — every legacy slug variant we accept points there.
        let caip2 = if network.starts_with("solana:") {
            network.to_string()
        } else {
            crate::x402_proto::SOLANA_MAINNET.to_string()
        };
        let decimals = decimals_for(&symbol);
        return Some((caip2, symbol, decimals));
    }
    {
        if let Some(chain_id_str) = network.strip_prefix("eip155:")
            && let Ok(chain_id) = chain_id_str.parse::<u64>()
            && let Some(symbol) = crate::client::balance::evm_symbol_for(chain_id, asset)
            && let Some(decimals) = crate::client::balance::evm_stablecoin_decimals(symbol)
        {
            return Some((network.to_string(), symbol.to_string(), decimals));
        }
    }
    let _ = (network, asset); // silence unused warnings in no-evm builds
    None
}

// ── Types ────────────────────────────────────────────────────────────────────

/// Configuration for a probe run.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Accepted currency symbols (e.g. ["USDC", "USDT"]).
    pub accepted_currencies: Vec<String>,
    /// Per-endpoint timeout in seconds.
    pub timeout_secs: u64,
    /// Max concurrent provider probes.
    pub concurrency: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            accepted_currencies: vec!["USDC".into(), "USDT".into()],
            timeout_secs: 10,
            concurrency: 5,
        }
    }
}

/// Aggregated metadata extracted from a 402 response.
///
/// Captures every indexable payment option advertised across all MPP
/// challenges and all x402 `accepts` entries — *not* just the one Pay would
/// settle on. Downstream tooling uses this to populate per-endpoint
/// `pricing`, `protocol[]`, `supported_usd[]`, and per-chain offers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PaidEndpoint {
    /// Payment protocols advertised, sorted: any of `["mpp", "x402"]`.
    pub protocols: Vec<String>,
    /// USD-pegged stablecoin symbols advertised, sorted unique. Union of every
    /// chain offered (Solana + EVM).
    pub supported_usd: Vec<String>,
    /// Canonical USD price (cheapest USDC tier across offers, else any
    /// stable). `None` when the response carried no indexable payment info.
    pub price_usd: Option<f64>,
    /// All distinct recipient addresses advertised across protocols/chains.
    pub recipients: Vec<String>,
    /// Endpoint description sourced (in priority order) from x402
    /// `resource.description`, the bazaar input description, or the
    /// MPP challenge/request description. Empty when none useful.
    pub description: Option<String>,
    /// True when the 402 response advertises a sign-in-with-x extension and no
    /// payment-acceptable scheme — i.e. the endpoint is gated by SIWX
    /// auth, not by a stablecoin payment.
    pub siwx_required: bool,
    /// Per-(network, asset) advertised offers — Phase 15. Multi-chain servers
    /// (Solana + EVM, or several EVM chains) emit one entry per accepts/MPP
    /// challenge. Empty for endpoints whose 402 envelope had no indexable
    /// option; serialization is skipped in that case so older index
    /// consumers that pre-date this field don't see a noisy empty array.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain_offers: Vec<ChainOffer>,
}

/// One advertised payment option in a 402 response, scoped to a specific
/// chain and token. Always uses CAIP-2 for `network` so downstream consumers
/// can ignore pay-internal slug variations.
#[derive(Debug, Clone, Serialize)]
pub struct ChainOffer {
    /// CAIP-2 chain identifier — `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`
    /// or `eip155:8453`. Never empty.
    pub network: String,
    /// Normalized token symbol (USDC, USDT, …).
    pub currency: String,
    /// Token contract address — Solana SPL mint or ERC-20 address. Stored
    /// verbatim from the envelope (no case normalization).
    pub asset: String,
    /// Receiving address.
    pub recipient: String,
    /// Base-unit integer string (no decimals applied).
    pub amount_raw: String,
    /// `amount_raw / 10^decimals`, or `None` if the token's decimals aren't
    /// known to pay (so callers don't read a 0.0 fallback as "free").
    pub price_usd: Option<f64>,
}

/// Result of probing a single endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct EndpointProbeResult {
    pub method: String,
    pub path: String,
    pub url: String,
    /// Existing high-level outcome — Solana stable + accepted currency = Ok.
    pub status: ProbeStatus,
    /// Full Solana-compatible payment metadata extracted from the 402.
    pub paid: PaidEndpoint,
    /// Stable string name for the probe outcome — see [`probe_status_str`].
    pub probe_status: String,
    /// Raw HTTP status code returned by the endpoint.
    pub http_status: u16,
    pub duration_ms: u64,
}

/// Outcome of a single endpoint probe.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Valid 402 challenge with accepted currency on Solana.
    Ok {
        protocol: String,
        currency: String,
        network: String,
        recipient: String,
    },
    /// 402 returned but only for non-Solana chains.
    WrongChain { details: String },
    /// 402 returned with a currency not in the accepted set.
    WrongCurrency { got: String, accepted: Vec<String> },
    /// 402 returned but no recognized payment protocol.
    UnknownProtocol,
    /// Endpoint did not return 402 (e.g. 200, 401, 500).
    NotPaywalled { status_code: u16 },
    /// Free endpoint (no pricing in the spec) — skipped.
    Free,
    /// Connection error or timeout.
    Error { message: String },
}

impl ProbeStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. } | Self::Free)
    }
}

/// Result of probing all endpoints for a single provider.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderProbeResult {
    pub fqn: String,
    pub service_url: String,
    pub endpoints: Vec<EndpointProbeResult>,
    pub pass: bool,
}

/// Aggregate result of probing multiple providers.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub providers: Vec<ProviderProbeResult>,
    pub total_endpoints: usize,
    pub passed: usize,
    pub failed: usize,
}

// ── Rich extraction ──────────────────────────────────────────────────────────

/// Walk every MPP challenge in `headers` and every x402 `accepts` entry in
/// `body`; record all Solana-compatible payment options.
///
/// MPP responses can advertise multiple challenges in a single `Payment`
/// auth-scheme header (e.g. one for `solana`, another for `tempo`). x402
/// responses ship a list under `accepts`. The runtime payment path picks one
/// to settle on, but the audit/index pipeline needs the full picture.
pub fn extract_paid_endpoint(headers: &[(String, String)], body: Option<&str>) -> PaidEndpoint {
    let mut paid = PaidEndpoint::default();

    // ── x402 envelope ──
    // Some servers (e.g. Vercel-hosted ones) ship an empty response body and
    // put the entire envelope in the `payment-required` header (base64 JSON).
    // Fall back to that when the body doesn't carry the envelope itself.
    let parsed_body: Option<Value> = body
        .and_then(|b| serde_json::from_str(b).ok())
        .or_else(|| parse_payment_required_header(headers));

    if let Some(json) = &parsed_body {
        // Walk accepts[] for every indexable (Solana or EVM stable) entry —
        // Phase 15 broadened this from Solana-only. Each match also pushes a
        // `ChainOffer` so downstream tooling can render per-chain options.
        let mut found_x402_offer = false;
        if let Some(accepts) = json.get("accepts").and_then(|v| v.as_array()) {
            for accept in accepts {
                let network = accept.get("network").and_then(|v| v.as_str()).unwrap_or("");
                let asset = accept.get("asset").and_then(|v| v.as_str()).unwrap_or("");
                let Some((caip2, symbol, decimals)) = resolve_offer(network, asset) else {
                    continue;
                };

                let amount_str = accept.get("amount").and_then(|v| v.as_str()).unwrap_or("0");
                let recipient = accept
                    .get("payTo")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let price_usd = amount_to_usd(amount_str, decimals);

                paid.chain_offers.push(ChainOffer {
                    network: caip2,
                    currency: symbol.clone(),
                    asset: asset.to_string(),
                    recipient: recipient.clone(),
                    amount_raw: amount_str.to_string(),
                    price_usd,
                });

                found_x402_offer = true;
                push_unique(&mut paid.supported_usd, &symbol);
                if let Some(usd) = price_usd {
                    update_canonical_price(&mut paid.price_usd, usd, &symbol);
                }
                if !recipient.is_empty() {
                    push_unique(&mut paid.recipients, &recipient);
                }
            }
        }
        if found_x402_offer {
            push_unique(&mut paid.protocols, "x402");
        }

        // Description — pick the first non-empty source.
        if paid.description.is_none() {
            paid.description = json
                .get("resource")
                .and_then(|r| r.get("description"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        if paid.description.is_none() {
            paid.description = json
                .pointer("/extensions/bazaar/info/input/description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }

        // SIWX detection: response advertises sign-in-with-x and accepts is
        // either missing or an empty array.
        let has_siwx = json
            .get("extensions")
            .and_then(|e| e.get("sign-in-with-x"))
            .is_some();
        let no_payment_options = json
            .get("accepts")
            .map(|v| v.as_array().is_some_and(|a| a.is_empty()))
            .unwrap_or(true);
        if has_siwx && no_payment_options && !found_x402_offer {
            paid.siwx_required = true;
        }
    }

    // ── MPP challenges (iterate ALL) ── (Solana-only)
    #[cfg(feature = "solana")]
    {
        let mut found_mpp_solana = false;
        let mut mpp_description: Option<String> = None;
        for challenge in crate::client::mpp::parse_headers(headers) {
            if !solana_mpp::client::is_solana_charge_challenge(&challenge) {
                continue;
            }
            let request: solana_mpp::ChargeRequest = match challenge.request.decode() {
                Ok(r) => r,
                Err(_) => continue,
            };
            let symbol = normalize_currency(&request.currency);
            if !is_usd_stable(&symbol) {
                continue;
            }
            found_mpp_solana = true;
            push_unique(&mut paid.supported_usd, &symbol);

            let decimals = decimals_for(&symbol);
            let price_usd = amount_to_usd(&request.amount, decimals);
            if let Some(usd) = price_usd {
                update_canonical_price(&mut paid.price_usd, usd, &symbol);
            }
            let recipient = request.recipient.clone().unwrap_or_default();
            if !recipient.is_empty() {
                push_unique(&mut paid.recipients, &recipient);
            }
            // Phase 15: MPP is Solana-only; surface the offer alongside x402
            // entries so downstream consumers see a unified per-chain list.
            paid.chain_offers.push(ChainOffer {
                network: crate::x402_proto::SOLANA_MAINNET.to_string(),
                currency: symbol.clone(),
                asset: request.currency.clone(),
                recipient,
                amount_raw: request.amount.clone(),
                price_usd,
            });
            if mpp_description.is_none() {
                mpp_description = request
                    .description
                    .as_deref()
                    .or(challenge.description.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
        }
        if found_mpp_solana {
            push_unique(&mut paid.protocols, "mpp");
            if paid.description.is_none() {
                paid.description = mpp_description;
            }
        }
    }

    paid.protocols.sort();
    paid.supported_usd.sort();
    paid.recipients.sort();
    paid.recipients.dedup();
    paid
}

/// Decode the base64 JSON envelope from the `payment-required` header, if any.
fn parse_payment_required_header(headers: &[(String, String)]) -> Option<Value> {
    use base64::Engine;
    let header_value = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("payment-required"))
        .map(|(_, v)| v.as_str())?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header_value.as_bytes())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    serde_json::from_str(&s).ok()
}

fn push_unique(vec: &mut Vec<String>, value: &str) {
    if !vec.iter().any(|v| v == value) {
        vec.push(value.to_string());
    }
}

/// Update `current` with `candidate` USD price, preferring the cheapest USDC
/// tier; fall back to any stable when no USDC observation exists yet.
fn update_canonical_price(current: &mut Option<f64>, candidate: f64, symbol: &str) {
    let is_usdc = symbol.eq_ignore_ascii_case("USDC");
    match current {
        None => *current = Some(candidate),
        Some(existing) => {
            if is_usdc && candidate < *existing {
                *current = Some(candidate);
            }
        }
    }
}

/// Stable string label for an endpoint probe. Used in the published index so
/// consumers can render "needs body", "auth required", "siwx required", etc.
fn probe_status_str(status: &ProbeStatus, http_status: u16, paid: &PaidEndpoint) -> &'static str {
    if paid.siwx_required {
        return "siwx_required";
    }
    match status {
        ProbeStatus::Ok { .. } => "ok",
        ProbeStatus::Free => "free",
        ProbeStatus::WrongChain { .. } => "wrong_chain",
        ProbeStatus::WrongCurrency { .. } => "wrong_currency",
        ProbeStatus::UnknownProtocol => "unknown_protocol",
        ProbeStatus::NotPaywalled { .. } => match http_status {
            401 | 403 => "auth_required",
            404 => "not_found",
            405 => "method_not_allowed",
            // 400 = server schema-validates the body before reaching the
            // paywall (e.g. stabledomains POST /api/register needs a domain).
            // 5xx = same shape with a less helpful error code.
            400 | 422 | 500..=599 => "unprobeable_needs_body",
            200..=299 => "free",
            _ => "not_paywalled",
        },
        ProbeStatus::Error { .. } => "error",
    }
}

// ── Probing ──────────────────────────────────────────────────────────────────

/// Probe a single endpoint and classify the response.
///
/// `body_override` is a pre-rendered JSON request body — typically derived
/// from the OpenAPI doc's `example` or schema. When `None` and the method
/// expects a body, we fall back to `{}`.
fn probe_endpoint(
    method: &str,
    url: &str,
    body_override: Option<&str>,
    config: &ProbeConfig,
) -> EndpointProbeResult {
    let start = Instant::now();

    let body = match method.to_uppercase().as_str() {
        "POST" | "PUT" | "PATCH" => Some(body_override.unwrap_or("{}")),
        _ => None,
    };
    let headers = if body.is_some() {
        vec![("content-type".into(), "application/json".into())]
    } else {
        vec![]
    };

    let raw = fetch_raw(method, url, &headers, body);
    let duration_ms = start.elapsed().as_millis() as u64;

    let (status, paid, probe_status, http_status) = match raw {
        Ok(raw) => {
            let body_text = raw.body_text();
            let content_type = raw.content_type().map(str::to_string);
            let outcome = if raw.status == 402 {
                runner::classify_402(&raw.headers, Some(&body_text), url)
            } else {
                let exit_code = if raw.status >= 400 { 1 } else { 0 };
                RunOutcome::Completed {
                    exit_code,
                    body: Some(raw.body.clone()),
                    content_type,
                }
            };
            let probe_status_kind = classify_outcome(outcome, &config.accepted_currencies);
            // Patch up NotPaywalled with the actual HTTP code (classify_outcome
            // collapses it to 200/500 since RunOutcome::Completed only tracks
            // exit_code, not the raw status).
            let probe_status_kind = match probe_status_kind {
                ProbeStatus::NotPaywalled { .. } => ProbeStatus::NotPaywalled {
                    status_code: raw.status,
                },
                other => other,
            };
            let paid = extract_paid_endpoint(&raw.headers, Some(&body_text));
            let label = probe_status_str(&probe_status_kind, raw.status, &paid);
            (probe_status_kind, paid, label.to_string(), raw.status)
        }
        Err(e) => {
            let label = "error".to_string();
            (
                ProbeStatus::Error {
                    message: e.to_string(),
                },
                PaidEndpoint::default(),
                label,
                0,
            )
        }
    };

    EndpointProbeResult {
        method: method.to_string(),
        path: String::new(), // filled in by caller
        url: url.to_string(),
        status,
        paid,
        probe_status,
        http_status,
        duration_ms,
    }
}

/// Map a `RunOutcome` to a `ProbeStatus`.
fn classify_outcome(outcome: RunOutcome, accepted: &[String]) -> ProbeStatus {
    match outcome {
        #[cfg(feature = "solana")]
        RunOutcome::MppChallenge { challenge, .. } => {
            let request: solana_mpp::ChargeRequest = match challenge.request.decode() {
                Ok(r) => r,
                Err(e) => {
                    return ProbeStatus::Error {
                        message: format!("Failed to decode MPP challenge: {e}"),
                    };
                }
            };

            let currency = normalize_currency(&request.currency);
            let network = request
                .method_details
                .as_ref()
                .and_then(|v| v.get("network"))
                .and_then(|v| v.as_str())
                .unwrap_or("mainnet")
                .to_string();
            let recipient = request.recipient.unwrap_or_default();

            if !accepted.iter().any(|a| a.eq_ignore_ascii_case(&currency)) {
                return ProbeStatus::WrongCurrency {
                    got: currency,
                    accepted: accepted.to_vec(),
                };
            }

            ProbeStatus::Ok {
                protocol: "mpp".into(),
                currency,
                network,
                recipient,
            }
        }

        #[cfg(feature = "solana")]
        RunOutcome::SessionChallenge { .. } => {
            // Session challenges are valid Solana endpoints but use a
            // different payment flow. Mark as ok with protocol "mpp-session".
            ProbeStatus::Ok {
                protocol: "mpp-session".into(),
                currency: "session".into(),
                network: "mainnet".into(),
                recipient: String::new(),
            }
        }

        RunOutcome::X402Challenge { challenge, .. } => {
            // Phase 15: pick whichever indexable accept matches the user's
            // build configuration first — EVM-first when the `evm` feature
            // is enabled (since the user explicitly opted in), Solana
            // otherwise. `paid.chain_offers` still carries every option, so
            // this only chooses the "primary display" for legacy flat
            // consumers of `ProbeStatus::Ok`.
            let candidates: &[_] = if challenge.all_accepts.is_empty() {
                std::slice::from_ref(&challenge.requirements)
            } else {
                &challenge.all_accepts
            };
            let chosen = match pick_indexable_x402(candidates) {
                Some(r) => r,
                None => {
                    return ProbeStatus::WrongChain {
                        details: format!(
                            "x402 envelope has no indexable accepts: {}",
                            candidates
                                .iter()
                                .map(|r| r.network.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    };
                }
            };

            // Normalize via `resolve_offer` so the displayed network is
            // always CAIP-2 — Solana entries that arrived as `"mainnet-beta"`
            // get pinned to the canonical genesis-hash form.
            let (caip2, symbol, _) = match resolve_offer(&chosen.network, &chosen.currency) {
                Some(t) => t,
                None => (
                    chosen
                        .cluster
                        .clone()
                        .unwrap_or_else(|| chosen.network.clone()),
                    normalize_currency(&chosen.currency),
                    decimals_for(&normalize_currency(&chosen.currency)),
                ),
            };
            let recipient = chosen.recipient.clone();

            if !accepted.iter().any(|a| a.eq_ignore_ascii_case(&symbol)) {
                return ProbeStatus::WrongCurrency {
                    got: symbol,
                    accepted: accepted.to_vec(),
                };
            }

            ProbeStatus::Ok {
                protocol: "x402".into(),
                currency: symbol,
                network: caip2,
                recipient,
            }
        }

        #[cfg(feature = "solana")]
        RunOutcome::X402SignInChallenge { .. } => ProbeStatus::Ok {
            protocol: "x402-siwx".into(),
            currency: "sign-in".into(),
            network: "mainnet".into(),
            recipient: String::new(),
        },

        RunOutcome::PaymentRejected { reason, .. } => ProbeStatus::WrongChain { details: reason },

        RunOutcome::UnknownPaymentRequired { .. } => ProbeStatus::UnknownProtocol,

        RunOutcome::Completed { exit_code, .. } => {
            // Non-402 response — could be 200 (free), 401, 403, 500, etc.
            // The caller patches the actual HTTP status code in.
            let status_code = if exit_code == 0 { 200 } else { 500 };
            ProbeStatus::NotPaywalled { status_code }
        }
    }
}

/// Probe all endpoints for a single provider.
pub fn probe_provider(
    provider: &pay_types::registry::ProbeProvider,
    config: &ProbeConfig,
) -> ProviderProbeResult {
    let mut results = Vec::with_capacity(provider.endpoints.len());

    for ep in &provider.endpoints {
        let url = format!(
            "{}/{}",
            provider.service_url.trim_end_matches('/'),
            ep.path.trim_start_matches('/')
        );
        let mut result = if ep.metered {
            probe_endpoint(&ep.method, &url, ep.body.as_deref(), config)
        } else {
            EndpointProbeResult {
                method: ep.method.clone(),
                path: String::new(),
                url: url.clone(),
                status: ProbeStatus::Free,
                paid: PaidEndpoint::default(),
                probe_status: "free".to_string(),
                http_status: 0,
                duration_ms: 0,
            }
        };
        result.path = ep.path.clone();
        results.push(result);
    }

    let pass = results.iter().all(|r| r.status.is_ok());

    ProviderProbeResult {
        fqn: provider.fqn.clone(),
        service_url: provider.service_url.clone(),
        endpoints: results,
        pass,
    }
}

/// Probe multiple providers concurrently.
pub fn probe_providers(
    providers: Vec<pay_types::registry::ProbeProvider>,
    config: &ProbeConfig,
) -> ProbeReport {
    let total_endpoints: usize = providers.iter().map(|p| p.endpoints.len()).sum();
    let results = std::sync::Mutex::new(Vec::with_capacity(providers.len()));
    let semaphore = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for provider in &providers {
            // Wait for a concurrency slot.
            loop {
                let current = semaphore.load(Ordering::Relaxed);
                if current < config.concurrency
                    && semaphore
                        .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::Relaxed)
                        .is_ok()
                {
                    break;
                }
                std::thread::yield_now();
            }

            let sem = &semaphore;
            let cfg = &config;
            let res = &results;

            scope.spawn(move || {
                let result = probe_provider(provider, cfg);
                res.lock().unwrap().push(result);
                sem.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    let providers = results.into_inner().unwrap();
    let passed = providers
        .iter()
        .flat_map(|p| &p.endpoints)
        .filter(|e| e.status.is_ok())
        .count();

    ProbeReport {
        providers,
        total_endpoints,
        passed,
        failed: total_endpoints - passed,
    }
}

// ── Helpers exposed for downstream consumers (e.g. build) ───────────────────

/// Synthesize an `EndpointSpec.pricing` JSON value from a probe result.
///
/// Returns:
/// - `Some({ "mode": "flat", "dimensions": [...] })` when we observed a USD
///   stablecoin price on Solana,
/// - `None` when no Solana payment was advertised (the caller can decide
///   whether to mark the endpoint free / unprobeable).
pub fn pricing_from_probe(paid: &PaidEndpoint) -> Option<Value> {
    let price = paid.price_usd?;
    Some(serde_json::json!({
        "mode": "flat",
        "dimensions": [{
            "direction": "usage",
            "unit": "requests",
            "scale": 1,
            "tiers": [{ "price_usd": price }]
        }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x402_body(accepts: Vec<Value>) -> String {
        serde_json::json!({
            "x402Version": 2,
            "error": "Payment Required!",
            "accepts": accepts,
            "resource": {
                "url": "https://example.com/api/foo",
                "description": "Test endpoint"
            }
        })
        .to_string()
    }

    #[test]
    fn extract_x402_solana_usdc_records_protocol_currency_price() {
        let body = x402_body(vec![serde_json::json!({
            "scheme": "exact",
            "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            "amount": "10000",
            "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "payTo": "BENrLoUbndxoNMUS5JXApGMtNykLjFXXixMtpDwDR9SP",
            "extra": {"name": "USDC"}
        })]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(paid.protocols, vec!["x402".to_string()]);
        assert_eq!(paid.supported_usd, vec!["USDC".to_string()]);
        assert_eq!(paid.price_usd, Some(0.01));
        assert_eq!(
            paid.recipients,
            vec!["BENrLoUbndxoNMUS5JXApGMtNykLjFXXixMtpDwDR9SP".to_string()]
        );
        assert_eq!(paid.description.as_deref(), Some("Test endpoint"));
        assert!(!paid.siwx_required);
    }

    #[test]
    fn extract_indexes_base_only_accepts_under_evm_feature() {
        // Phase 15: an EVM-only x402 envelope (Base USDC) is now indexable;
        // it shows up in `chain_offers` with the eip155 CAIP-2 network and
        // populates the legacy flat fields too. Solana-only builds skip
        // EVM entries naturally (the `#[cfg]` here makes that explicit).
        let body = x402_body(vec![serde_json::json!({
            "scheme": "exact",
            "network": "eip155:8453",
            "amount": "10000",
            "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "payTo": "0x0000000000000000000000000000000000000001",
        })]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(paid.protocols, vec!["x402".to_string()]);
        assert_eq!(paid.supported_usd, vec!["USDC".to_string()]);
        assert_eq!(paid.price_usd, Some(0.01));
        assert_eq!(paid.chain_offers.len(), 1);
        let offer = &paid.chain_offers[0];
        assert_eq!(offer.network, "eip155:8453");
        assert_eq!(offer.currency, "USDC");
        assert_eq!(offer.amount_raw, "10000");
    }

    #[test]
    fn extract_collects_multiple_solana_stables() {
        let body = x402_body(vec![
            serde_json::json!({
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "20000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "RecipientUSDC",
            }),
            serde_json::json!({
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "30000",
                "asset": "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
                "payTo": "RecipientUSDT",
            }),
        ]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(
            paid.supported_usd,
            vec!["USDC".to_string(), "USDT".to_string()]
        );
        // Canonical price prefers USDC tier (0.02) over USDT (0.03)
        assert_eq!(paid.price_usd, Some(0.02));
        assert_eq!(paid.recipients.len(), 2);
    }

    #[test]
    fn extract_canonical_price_picks_cheapest_usdc_tier() {
        let body = x402_body(vec![
            serde_json::json!({
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "50000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "ATA1",
            }),
            serde_json::json!({
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "10000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "ATA2",
            }),
        ]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(paid.price_usd, Some(0.01));
    }

    #[test]
    fn extract_detects_siwx_only_endpoint() {
        let body = serde_json::json!({
            "x402Version": 2,
            "error": "SIWX authentication required",
            "accepts": [],
            "resource": {
                "url": "https://example.com/api/list",
                "description": "SIWX-protected endpoint"
            },
            "extensions": {
                "sign-in-with-x": {
                    "info": {"chainId": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"}
                }
            }
        })
        .to_string();
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert!(paid.siwx_required);
        assert!(paid.protocols.is_empty());
        assert_eq!(paid.price_usd, None);
    }

    #[test]
    fn extract_uses_bazaar_description_when_resource_description_missing() {
        let body = serde_json::json!({
            "x402Version": 2,
            "accepts": [{
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "1000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "X",
            }],
            "extensions": {
                "bazaar": {
                    "info": {
                        "input": { "description": "Search by keyword" }
                    }
                }
            }
        })
        .to_string();
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(paid.description.as_deref(), Some("Search by keyword"));
    }

    #[test]
    fn extract_handles_empty_body_and_headers() {
        let paid = extract_paid_endpoint(&[], None);
        assert!(paid.protocols.is_empty());
        assert!(paid.supported_usd.is_empty());
        assert_eq!(paid.price_usd, None);
        assert!(!paid.siwx_required);
    }

    #[test]
    fn pricing_from_probe_emits_flat_dimensions() {
        let paid = PaidEndpoint {
            price_usd: Some(0.01),
            ..Default::default()
        };
        let pricing = pricing_from_probe(&paid).unwrap();
        assert_eq!(pricing["mode"], "flat");
        assert_eq!(pricing["dimensions"][0]["unit"], "requests");
        assert_eq!(pricing["dimensions"][0]["tiers"][0]["price_usd"], 0.01);
    }

    #[test]
    fn pricing_from_probe_returns_none_when_no_price() {
        let paid = PaidEndpoint::default();
        assert!(pricing_from_probe(&paid).is_none());
    }

    #[test]
    fn amount_to_usd_handles_typical_usdc_amounts() {
        assert_eq!(amount_to_usd("10000", 6), Some(0.01));
        assert_eq!(amount_to_usd("1000000", 6), Some(1.0));
        assert_eq!(amount_to_usd("0", 6), Some(0.0));
        assert_eq!(amount_to_usd("not a number", 6), None);
    }

    // ── Phase 15 — multi-chain index publication ─────────────────────────

    #[test]
    fn extract_merges_solana_and_evm_offers_into_chain_offers() {
        // A multichain endpoint advertising Solana USDC + Base USDC should
        // produce two chain_offers (one per network) but a single union
        // `supported_usd` and the cheaper price as the canonical flat
        // `price_usd`. Legacy flat fields stay populated for back-compat.
        let body = x402_body(vec![
            serde_json::json!({
                "scheme": "exact",
                "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "20000",
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "SolanaRecipient",
            }),
            serde_json::json!({
                "scheme": "exact",
                "network": "eip155:8453",
                "amount": "10000",
                "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                "payTo": "0x0000000000000000000000000000000000000001",
            }),
        ]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert_eq!(paid.protocols, vec!["x402".to_string()]);
        assert_eq!(paid.supported_usd, vec!["USDC".to_string()]);
        assert_eq!(paid.price_usd, Some(0.01)); // Base 0.01 < Solana 0.02
        assert_eq!(paid.chain_offers.len(), 2);
        assert!(paid
            .chain_offers
            .iter()
            .any(|o| o.network.starts_with("solana:") && o.amount_raw == "20000"));
        assert!(paid
            .chain_offers
            .iter()
            .any(|o| o.network == "eip155:8453" && o.amount_raw == "10000"));
    }

    #[test]
    fn extract_skips_unknown_evm_token() {
        // A random ERC-20 address we don't know shouldn't pollute the index
        // — chain_offers stays empty, no protocol claimed.
        let body = x402_body(vec![serde_json::json!({
            "scheme": "exact",
            "network": "eip155:8453",
            "amount": "10000",
            "asset": "0xDEADBEEF00000000000000000000000000000000",
            "payTo": "0x0000000000000000000000000000000000000001",
        })]);
        let paid = extract_paid_endpoint(&[], Some(&body));
        assert!(paid.chain_offers.is_empty());
        assert!(paid.protocols.is_empty());
        assert!(paid.supported_usd.is_empty());
    }

    #[test]
    fn classify_outcome_accepts_evm_only_x402_when_stable_known() {
        // Pre-Phase-15 this returned `WrongChain`; Phase 15 indexes it as
        // `Ok` with the eip155 CAIP-2 surfaced via `network`.
        use crate::client::runner::RunOutcome;
        use crate::x402_proto::PaymentRequirements;
        let req = PaymentRequirements {
            network: "eip155:8453".to_string(),
            cluster: None,
            recipient: "0x0000000000000000000000000000000000000001".to_string(),
            amount: "10000".to_string(),
            currency: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string(),
            decimals: Some(6),
            token_program: None,
            resource: "https://example.com/r".to_string(),
            description: None,
            max_age: Some(300),
            recent_blockhash: None,
            fee_payer: None,
            fee_payer_key: None,
            extra: None,
            accepted: None,
            resource_info: None,
        };
        let challenge = crate::client::x402::Challenge {
            x402_version: 2,
            requirements: req.clone(),
            all_accepts: vec![req.clone()],
            siwx: None,
        };
        let outcome = RunOutcome::X402Challenge {
            challenge: Box::new(challenge),
            resource_url: "https://example.com/r".to_string(),
        };
        let status = classify_outcome(outcome, &["USDC".to_string()]);
        match status {
            ProbeStatus::Ok {
                protocol,
                currency,
                network,
                ..
            } => {
                assert_eq!(protocol, "x402");
                assert_eq!(currency, "USDC");
                assert_eq!(network, "eip155:8453");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_prefers_evm_when_envelope_offers_both() {
        // EVM-first: when an envelope offers both Solana and Base, the
        // probe surfaces the Base entry as the primary `ProbeStatus::Ok::network`.
        use crate::client::runner::RunOutcome;
        use crate::x402_proto::PaymentRequirements;
        let solana_req = PaymentRequirements {
            network: "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp".to_string(),
            cluster: None,
            recipient: "SolanaRecipient".to_string(),
            amount: "10000".to_string(),
            currency: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            decimals: Some(6),
            token_program: None,
            resource: "r".into(),
            description: None,
            max_age: Some(300),
            recent_blockhash: None,
            fee_payer: None,
            fee_payer_key: None,
            extra: None,
            accepted: None,
            resource_info: None,
        };
        let evm_req = PaymentRequirements {
            network: "eip155:8453".to_string(),
            cluster: None,
            recipient: "0x0000000000000000000000000000000000000001".to_string(),
            amount: "10000".to_string(),
            currency: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string(),
            decimals: Some(6),
            token_program: None,
            resource: "r".into(),
            description: None,
            max_age: Some(300),
            recent_blockhash: None,
            fee_payer: None,
            fee_payer_key: None,
            extra: None,
            accepted: None,
            resource_info: None,
        };
        let challenge = crate::client::x402::Challenge {
            x402_version: 2,
            requirements: solana_req.clone(),
            all_accepts: vec![solana_req, evm_req],
            siwx: None,
        };
        let outcome = RunOutcome::X402Challenge {
            challenge: Box::new(challenge),
            resource_url: "r".into(),
        };
        let status = classify_outcome(outcome, &["USDC".to_string()]);
        match status {
            ProbeStatus::Ok { network, .. } => {
                assert_eq!(network, "eip155:8453", "EVM build must surface EVM entry first");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn paid_endpoint_serde_omits_empty_chain_offers() {
        // Back-compat: index files written before Phase 15 are
        // `chain_offers`-less. Serializing a PaidEndpoint with no offers
        // must NOT emit the field, so the on-disk schema stays the same
        // for endpoints that have no payment options to advertise.
        let paid = PaidEndpoint::default();
        let json = serde_json::to_string(&paid).expect("serialize");
        assert!(
            !json.contains("chain_offers"),
            "empty chain_offers should be skipped, got: {json}"
        );
    }
}
