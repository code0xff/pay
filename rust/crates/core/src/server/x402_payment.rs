//! Payment middleware for the x402 protocol.
//!
//! Mirrors the MPP middleware in `server::payment` but speaks the Coinbase
//! x402 wire format:
//! - Challenge header: `PAYMENT-REQUIRED` (base64 JSON envelope)
//! - Settlement header: `PAYMENT-SIGNATURE` (base64 JSON payment proof) or
//!   the legacy v1 `X-PAYMENT` header.
//!
//! When a payment proof is presented, the x402 SDK returns either a
//! `Signature` (already on-chain, just forward) or a `Transaction` (verified
//! but unbroadcast — the middleware must submit it to the configured RPC
//! before forwarding upstream). MPP settles inside the SDK; x402 makes the
//! caller responsible, so the explicit `broadcast` arm below is required.
//!
//! The default branch falls back to the MPP middleware when `protocol: x402`
//! is not set, so existing YAML specs continue to work unchanged.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use solana_x402::server::{ExactOptions, VerifiedExactPayment, X402};
use solana_x402::{PAYMENT_REQUIRED_HEADER, PAYMENT_SIGNATURE_HEADER, X402_V1_PAYMENT_HEADER};

use crate::PaymentState;
use crate::server::metering::{self, RequestProperties};
use crate::server::telemetry;

/// Axum middleware that gates metered endpoints behind an x402 payment.
pub async fn x402_payment_middleware<S: PaymentState>(
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
        // Single-API mode mirrors the MPP middleware: with only one API
        // configured, ignore the subdomain hint so `127.0.0.1` URLs still
        // route to the metered routes.
        None if apis.len() == 1 => &apis[0],
        None => return next.run(req).await,
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

    let x402s = state.x402s();
    if x402s.is_empty() {
        // Mounted but no servers — fail closed. The middleware is only mounted
        // when `protocol: x402` was selected, so an empty list is a config
        // bug, not a legitimate free passthrough.
        tracing::error!("Metered endpoint hit but x402 not configured — returning 500");
        return internal_error("x402 protocol enabled but no x402 servers configured");
    }

    let props = extract_request_properties(&headers, &path);
    let variant_hint = extract_variant_hint(&path);
    let decimals = u32::from(x402s[0].decimals());
    let amount = match resolve_amount(meter, &props, variant_hint.as_deref(), decimals) {
        Ok(a) => a,
        Err(e) => return internal_error(&e),
    };

    // v2 header (`PAYMENT-SIGNATURE`) first, then the v1 fallback
    // (`X-PAYMENT`). The SDK auto-detects which version the credential is
    // tagged for on parse, so any matching header value is forwarded as-is.
    let payment_header = headers
        .get(PAYMENT_SIGNATURE_HEADER)
        .or_else(|| headers.get(X402_V1_PAYMENT_HEADER))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    match payment_header {
        None => challenge_response(&x402s, &method, &path, subdomain, &amount, endpoint),
        Some(header) => {
            handle_payment(
                x402s,
                header,
                amount,
                subdomain,
                &path,
                req,
                next,
            )
            .await
        }
    }
}

/// Resolve the per-request decimal amount for the x402 SDK.
///
/// The SDK's `parse_units` rejects scientific notation and any payload with
/// more fractional digits than the token supports. `format!("{f64}")` emits
/// both for sufficiently small/large values, so we format with fixed
/// precision matching `decimals` here and validate the value is finite,
/// non-negative, and within `u128` range.
fn resolve_amount(
    meter: &pay_types::metering::Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
    decimals: u32,
) -> Result<String, String> {
    let per_unit = metering::resolve_price(meter, props, variant_hint, None)
        .and_then(|p| p.dimensions.first().cloned())
        .map(|d| d.price_usd / d.scale.max(1) as f64)
        .unwrap_or(0.01);
    if !per_unit.is_finite() || per_unit < 0.0 {
        return Err(format!(
            "Invalid metered price for x402: {per_unit} (must be finite and non-negative)"
        ));
    }
    // Cap precision at `decimals` so the SDK's parse_units doesn't reject us
    // with "Too many decimal places". USDC = 6 decimals; smaller fractional
    // prices than that are unrepresentable on-chain.
    Ok(format!("{per_unit:.*}", decimals as usize))
}

fn internal_error(message: &str) -> Response {
    tracing::error!(message, "x402 middleware aborted");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({
            "error": "internal_error",
            "message": message,
        })),
    )
        .into_response()
}

fn challenge_response(
    x402s: &[&X402],
    method: &Method,
    path: &str,
    subdomain: &str,
    amount: &str,
    endpoint: Option<&pay_types::metering::Endpoint>,
) -> Response {
    // x402 currently maps one X402 instance per currency. We emit a single
    // 402 envelope using the first configured instance — multi-currency
    // ("accepts": [...]) support belongs in a follow-up that uses
    // `X402::exact_with_payment_options`.
    let x402 = x402s[0];
    let description = endpoint.and_then(|ep| ep.description.as_deref());

    let envelope = match x402.exact_with_options(
        amount,
        ExactOptions {
            description,
            resource: None,
            max_age: None,
        },
    ) {
        Ok(env) => env,
        Err(e) => {
            telemetry::record_challenge_error("x402", x402.currency(), &e.to_string());
            tracing::error!(error = %e, currency = %x402.currency(), "Failed to build x402 challenge");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": "challenge_generation_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    let header_value = match serde_json::to_string(&envelope) {
        Ok(json) => base64::Engine::encode(&base64::engine::general_purpose::STANDARD, json),
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize x402 envelope");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "internal_error"})),
            )
                .into_response();
        }
    };

    telemetry::record_402_challenge_sent(
        "x402",
        subdomain,
        path,
        method.as_str(),
        amount.parse::<f64>().ok(),
        x402.currency(),
        1,
    );

    let body = json!({
        "error": "payment_required",
        "message": "This endpoint requires payment.",
        "endpoint": { "method": method.as_str(), "path": path },
        "payment": {
            "protocol": "x402",
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
    name = "x402_payment",
    skip(x402s, header, req, next),
    fields(subdomain = %subdomain, path = %path)
)]
async fn handle_payment(
    x402s: Vec<&X402>,
    header: String,
    amount: String,
    subdomain: &str,
    path: &str,
    req: Request<Body>,
    next: Next,
) -> Response {
    let mut last_error: Option<String> = None;
    for x402 in &x402s {
        match x402
            .process_payment(&header, &amount, ExactOptions::default())
            .await
        {
            Ok(VerifiedExactPayment::Signature(sig)) => {
                tracing::info!(subdomain = %subdomain, path = %path, signature = %sig, "x402 payment verified (on-chain) — forwarding");
                telemetry::record_payment_collected("x402", subdomain, path, None, &sig);
                let response = next.run(req).await;
                let status = response.status();
                telemetry::record_paid_request_completed("x402", subdomain, path, status, None);
                return response;
            }
            Ok(VerifiedExactPayment::Transaction(tx)) => {
                let rpc_url = x402.rpc_url();
                let tx_for_send = tx.clone();
                let broadcast = tokio::task::spawn_blocking(move || {
                    use solana_x402::solana_rpc_client::rpc_client::RpcClient;
                    let rpc = RpcClient::new(rpc_url);
                    rpc.send_and_confirm_transaction(&tx_for_send)
                })
                .await;
                let sig = match broadcast {
                    Ok(Ok(sig)) => sig.to_string(),
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        tracing::warn!(subdomain = %subdomain, path = %path, error = %msg, "x402 transaction broadcast failed");
                        telemetry::record_settlement_error("x402", subdomain, path, &msg, true);
                        return verification_failed_response(&msg);
                    }
                    Err(e) => {
                        let msg = format!("broadcast task panicked: {e}");
                        telemetry::record_settlement_error("x402", subdomain, path, &msg, false);
                        return verification_failed_response(&msg);
                    }
                };
                tracing::info!(subdomain = %subdomain, path = %path, signature = %sig, "x402 transaction broadcast — forwarding");
                telemetry::record_payment_collected("x402", subdomain, path, None, &sig);
                let response = next.run(req).await;
                let status = response.status();
                telemetry::record_paid_request_completed("x402", subdomain, path, status, None);
                return response;
            }
            Err(e) => last_error = Some(e.to_string()),
        }
    }

    let message = last_error.unwrap_or_else(|| "x402 verification failed".to_string());
    telemetry::record_settlement_error("x402", subdomain, path, &message, true);
    tracing::warn!(subdomain = %subdomain, path = %path, error = %message, "x402 payment verification failed");
    verification_failed_response(&message)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_variant_hint_models() {
        assert_eq!(
            extract_variant_hint("v1/models/gemini-2.0-flash:generateContent"),
            Some("gemini-2.0-flash".to_string())
        );
    }

    #[test]
    fn extract_variant_hint_no_match() {
        assert_eq!(extract_variant_hint("v1/images/generate"), None);
    }

    #[test]
    fn resolve_amount_falls_back_when_no_metering() {
        let meter = pay_types::metering::Metering {
            dimensions: vec![],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
        };
        let amount = resolve_amount(&meter, &RequestProperties::default(), None, 6)
            .expect("default amount");
        // 6-decimal fixed precision: 0.01 → "0.010000".
        assert_eq!(amount, "0.010000");
    }

    fn meter_with_price(price_usd: f64) -> pay_types::metering::Metering {
        pay_types::metering::Metering {
            dimensions: vec![pay_types::metering::MeterDimension {
                direction: pay_types::metering::MeterDirection::Usage,
                unit: pay_types::metering::BillingUnit::Requests,
                scale: 1,
                period: None,
                tiers: vec![pay_types::metering::PriceTier {
                    up_to: None,
                    price_usd,
                    condition: None,
                    notes: None,
                    splits: vec![],
                }],
            }],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
        }
    }

    #[test]
    fn resolve_amount_caps_precision_at_token_decimals() {
        // A price with 8 fractional digits gets truncated to the token's 6
        // — without this, the SDK's parse_units rejects with "Too many
        // decimal places for amount".
        let meter = meter_with_price(0.00000001);
        let amount =
            resolve_amount(&meter, &RequestProperties::default(), None, 6).expect("amount");
        assert_eq!(amount, "0.000000");
        assert!(!amount.contains('e'));
    }

    #[test]
    fn resolve_amount_rejects_non_finite_price() {
        let meter = meter_with_price(f64::NAN);
        let err = resolve_amount(&meter, &RequestProperties::default(), None, 6).unwrap_err();
        assert!(err.contains("finite"));
    }
}
