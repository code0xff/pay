//! HTTP client for the canonical x402 facilitator service.
//!
//! EVM x402 settlement requires the gateway to broadcast a
//! `transferWithAuthorization` ERC-20 call signed by the payer — paying ETH
//! gas itself in the process. The gateway delegates that work to an external
//! facilitator (e.g. `https://facilitator.x402.org`) that owns an EVM hot
//! wallet and exposes two HTTP endpoints:
//!
//! - `POST /verify` — checks the credential's signature against the route's
//!   requirements without broadcasting.
//! - `POST /settle` — verifies and broadcasts the transferWithAuthorization
//!   transaction; returns the resulting tx hash.
//!
//! Both endpoints accept the same JSON body shape:
//! ```json
//! { "paymentPayload": <decoded x402 signature envelope>,
//!   "paymentRequirements": <PaymentRequirements> }
//! ```

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FacilitatorError {
    #[error("facilitator HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("facilitator returned non-2xx ({status}): {body}")]
    Status { status: u16, body: String },
    #[error("facilitator returned malformed JSON: {0}")]
    Decode(String),
    #[error("facilitator marked payment invalid: {0}")]
    Invalid(String),
    #[error("facilitator failed to settle: {0}")]
    Settle(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    pub is_valid: bool,
    #[serde(default)]
    pub invalid_reason: Option<String>,
    #[serde(default)]
    pub payer: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleResponse {
    pub success: bool,
    #[serde(default)]
    pub error_reason: Option<String>,
    #[serde(default)]
    pub transaction: Option<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub payer: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FacilitatorRequest<'a> {
    payment_payload: &'a serde_json::Value,
    payment_requirements: &'a serde_json::Value,
}

/// Thin reqwest-based client for the canonical x402 facilitator API.
#[derive(Clone)]
pub struct FacilitatorClient {
    base_url: String,
    http: reqwest::Client,
}

impl FacilitatorClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder always succeeds with default config");
        Self { base_url, http }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn verify(
        &self,
        payment_payload: &serde_json::Value,
        requirements: &serde_json::Value,
    ) -> Result<VerifyResponse, FacilitatorError> {
        let body = FacilitatorRequest {
            payment_payload,
            payment_requirements: requirements,
        };
        let resp = self
            .http
            .post(format!("{}/verify", self.base_url))
            .json(&body)
            .send()
            .await?;
        decode_response::<VerifyResponse>(resp).await
    }

    pub async fn settle(
        &self,
        payment_payload: &serde_json::Value,
        requirements: &serde_json::Value,
    ) -> Result<SettleResponse, FacilitatorError> {
        let body = FacilitatorRequest {
            payment_payload,
            payment_requirements: requirements,
        };
        let resp = self
            .http
            .post(format!("{}/settle", self.base_url))
            .json(&body)
            .send()
            .await?;
        decode_response::<SettleResponse>(resp).await
    }
}

async fn decode_response<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, FacilitatorError> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(FacilitatorError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    serde_json::from_str::<T>(&text).map_err(|e| FacilitatorError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_url_strips_trailing_slash() {
        let client = FacilitatorClient::new("https://example.test/");
        assert_eq!(client.base_url(), "https://example.test");
    }

    #[test]
    fn verify_response_decodes_camelcase_payload() {
        let raw = json!({
            "isValid": true,
            "invalidReason": null,
            "payer": "0xabc"
        });
        let parsed: VerifyResponse = serde_json::from_value(raw).expect("decode");
        assert!(parsed.is_valid);
        assert_eq!(parsed.payer.as_deref(), Some("0xabc"));
    }

    #[test]
    fn settle_response_decodes_tx_hash() {
        let raw = json!({
            "success": true,
            "transaction": "0xdeadbeef",
            "network": "eip155:8453",
            "payer": "0xabc"
        });
        let parsed: SettleResponse = serde_json::from_value(raw).expect("decode");
        assert!(parsed.success);
        assert_eq!(parsed.transaction.as_deref(), Some("0xdeadbeef"));
    }
}
