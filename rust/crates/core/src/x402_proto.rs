//! x402 protocol wire types — chain-neutral.
//!
//! Phase 21 extracted these from `solana_x402::exact` so the EVM/x402 path
//! does not pull the Solana SDK into the build graph. The types preserve the
//! exact field names and serde behavior of the original so existing wire
//! formats round-trip 1-to-1.
//!
//! Solana payment building still goes through `solana_x402::client::exact`
//! (gated by `feature = "solana"`); see `crate::client::x402` for the
//! `From<&PaymentRequirements>` conversion that bridges our type to the
//! SDK's at the call site.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ── Constants ───────────────────────────────────────────────────────────────

/// Generic "solana" shorthand used when no CAIP-2 cluster is supplied.
pub const SOLANA_NETWORK: &str = "solana";

/// JSON field carrying the wire-protocol version (`"x402Version"`).
pub const X402_VERSION_FIELD: &str = "x402Version";

/// x402 v1 numeric version.
pub const X402_VERSION_V1: u64 = 1;

/// x402 v2 numeric version.
pub const X402_VERSION_V2: u64 = 2;

// v1 wire headers
pub const X402_V1_PAYMENT_HEADER: &str = "X-PAYMENT";
pub const X402_V1_PAYMENT_REQUIRED_HEADER: &str = "X-PAYMENT-REQUIRED";
pub const X402_V1_PAYMENT_RESPONSE_HEADER: &str = "X-PAYMENT-RESPONSE";

// v2 wire headers
pub const X402_V2_PAYMENT_HEADER: &str = "PAYMENT-SIGNATURE";
pub const X402_V2_PAYMENT_REQUIRED_HEADER: &str = "PAYMENT-REQUIRED";
pub const X402_V2_PAYMENT_RESPONSE_HEADER: &str = "PAYMENT-RESPONSE";

// SIWX (Sign-In With X) headers
pub const SIGN_IN_WITH_X: &str = "sign-in-with-x";
pub const SIGN_IN_WITH_X_HEADER: &str = "SIGN-IN-WITH-X";

// Convenience aliases (point at the v2 headers)
pub const PAYMENT_REQUIRED_HEADER: &str = X402_V2_PAYMENT_REQUIRED_HEADER;
pub const PAYMENT_SIGNATURE_HEADER: &str = X402_V2_PAYMENT_HEADER;
pub const PAYMENT_RESPONSE_HEADER: &str = X402_V2_PAYMENT_RESPONSE_HEADER;

/// x402 `exact` scheme identifier.
pub const EXACT_SCHEME: &str = "exact";

// CAIP-2 Solana cluster identifiers. Kept here as protocol-level strings;
// they do not depend on the Solana SDK.
pub const SOLANA_MAINNET: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";
pub const SOLANA_DEVNET: &str = "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1";
pub const SOLANA_TESTNET: &str = "solana:4uhcVJyU9pJkvQyS88uRDiswHXSCkY3z";

// ── ResourceInfo ────────────────────────────────────────────────────────────

/// Resource metadata carried by canonical x402 v2 payment-required responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

// ── PaymentRequirements ────────────────────────────────────────────────────

/// x402 `exact` scheme payment requirements (wire shape).
///
/// Mirrors `solana_x402::exact::PaymentRequirements` byte-for-byte so the
/// same wire payload deserializes to the same struct regardless of whether
/// the Solana SDK is in the build graph. Some fields are Solana-only
/// (`cluster`, `token_program`, `recent_blockhash`, etc.); they stay
/// `None` on the EVM path.
#[derive(Debug, Clone)]
pub struct PaymentRequirements {
    /// CAIP-2 network identifier.
    pub network: String,
    /// Solana cluster: mainnet-beta, devnet, or localnet.
    pub cluster: Option<String>,
    /// Recipient address (base58 for Solana, EIP-55 hex for EVM).
    pub recipient: String,
    /// Amount in base units (lamports or token smallest unit).
    pub amount: String,
    /// Currency: ticker (`"USDC"`) or address (`"0x..."` / mint).
    pub currency: String,
    /// Token decimals (required for SPL/ERC-20 tokens).
    pub decimals: Option<u8>,
    /// Token program address (Solana SPL).
    pub token_program: Option<String>,
    /// Unique resource identifier for this payment.
    pub resource: String,
    /// Human-readable description of what is being paid for.
    pub description: Option<String>,
    /// Maximum age in seconds for the payment to remain valid.
    pub max_age: Option<u64>,
    /// Server-provided recent blockhash (Solana).
    pub recent_blockhash: Option<String>,
    /// If true, server pays transaction fees (Solana fee payer).
    pub fee_payer: Option<bool>,
    /// Server's fee payer public key (Solana).
    pub fee_payer_key: Option<String>,
    /// Extra protocol-specific data.
    pub extra: Option<serde_json::Value>,
    /// Original canonical accepted object from a v2 challenge, when parsed.
    pub accepted: Option<serde_json::Value>,
    /// Original v2 resource metadata, when parsed.
    pub resource_info: Option<ResourceInfo>,
}

impl PaymentRequirements {
    /// Canonical v2 accepted object for this requirement.
    pub fn to_accepted_value(&self) -> serde_json::Value {
        if let Some(accepted) = &self.accepted {
            return accepted.clone();
        }
        serde_json::json!({
            "scheme": EXACT_SCHEME,
            "network": self.network.clone(),
            "amount": self.amount.clone(),
            "asset": self.currency.clone(),
            "payTo": self.recipient.clone(),
            "maxTimeoutSeconds": self.max_age.unwrap_or(300),
            "extra": self.canonical_extra_value(),
        })
    }

    /// Canonical v2 resource object associated with this requirement.
    pub fn resource_info(&self) -> Option<ResourceInfo> {
        self.resource_info.clone().or_else(|| {
            if self.resource.is_empty() {
                None
            } else {
                Some(ResourceInfo {
                    url: self.resource.clone(),
                    description: self.description.clone(),
                    mime_type: None,
                })
            }
        })
    }

    fn canonical_extra_value(&self) -> serde_json::Value {
        let mut extra = self
            .extra
            .as_ref()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();

        if let Some(fee_payer) = &self.fee_payer_key {
            extra
                .entry("feePayer".to_string())
                .or_insert_with(|| serde_json::Value::String(fee_payer.clone()));
        }
        if let Some(recent_blockhash) = &self.recent_blockhash {
            extra
                .entry("recentBlockhash".to_string())
                .or_insert_with(|| serde_json::Value::String(recent_blockhash.clone()));
        }
        if let Some(token_program) = &self.token_program {
            extra
                .entry("tokenProgram".to_string())
                .or_insert_with(|| serde_json::Value::String(token_program.clone()));
        }
        if let Some(decimals) = self.decimals {
            extra
                .entry("decimals".to_string())
                .or_insert_with(|| serde_json::Value::from(decimals));
        }

        serde_json::Value::Object(extra)
    }
}

impl Serialize for PaymentRequirements {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_accepted_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PaymentRequirements {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("payment requirement must be an object"))?;

        let raw_network =
            string_field(object, "network").unwrap_or_else(|| SOLANA_NETWORK.to_string());
        let network = normalize_network_identifier(&raw_network);
        let cluster = string_field(object, "cluster").or_else(|| {
            cluster_for_caip2_network(&network).map(|cluster| {
                if raw_network.starts_with("solana:") {
                    raw_network.clone()
                } else {
                    cluster.to_string()
                }
            })
        });

        let extra = object.get("extra").cloned();
        let extra_object = extra.as_ref().and_then(|value| value.as_object());

        let recipient = string_field(object, "recipient")
            .or_else(|| string_field(object, "payTo"))
            .unwrap_or_default();
        let amount = string_field(object, "amount")
            .or_else(|| string_field(object, "maxAmountRequired"))
            .unwrap_or_default();
        let currency = string_field(object, "currency")
            .or_else(|| string_field(object, "asset"))
            .unwrap_or_else(|| "SOL".to_string());

        let decimals = u8_field(object, "decimals")
            .or_else(|| extra_object.and_then(|extra| u8_field(extra, "decimals")));
        let token_program = string_field(object, "tokenProgram")
            .or_else(|| extra_object.and_then(|extra| string_field(extra, "tokenProgram")));
        let recent_blockhash = string_field(object, "recentBlockhash")
            .or_else(|| extra_object.and_then(|extra| string_field(extra, "recentBlockhash")));
        let fee_payer_key = string_field(object, "feePayerKey")
            .or_else(|| extra_object.and_then(|extra| string_field(extra, "feePayer")));
        let fee_payer =
            bool_field(object, "feePayer").or_else(|| fee_payer_key.as_ref().map(|_| true));
        let max_age =
            u64_field(object, "maxAge").or_else(|| u64_field(object, "maxTimeoutSeconds"));

        let accepted = if object.contains_key("amount")
            && object.contains_key("asset")
            && object.contains_key("payTo")
        {
            Some(value.clone())
        } else {
            None
        };

        Ok(Self {
            network,
            cluster,
            recipient,
            amount,
            currency,
            decimals,
            token_program,
            resource: string_field(object, "resource").unwrap_or_default(),
            description: string_field(object, "description"),
            max_age,
            recent_blockhash,
            fee_payer,
            fee_payer_key,
            extra,
            accepted,
            resource_info: None,
        })
    }
}

// ── PaymentRequiredEnvelope ─────────────────────────────────────────────────

/// Wire envelope carried in `PAYMENT-REQUIRED`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequiredEnvelope {
    pub x402_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceInfo>,
    #[serde(default)]
    pub accepts: Vec<PaymentRequirements>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

impl PaymentRequiredEnvelope {
    /// Attach top-level v2 resource metadata to parsed accepts.
    pub fn with_resource_on_accepts(mut self) -> Self {
        if let Some(resource) = &self.resource {
            for accept in &mut self.accepts {
                accept.resource_info = Some(resource.clone());
                if accept.resource.is_empty() {
                    accept.resource = resource.url.clone();
                }
                if accept.description.is_none() {
                    accept.description = resource.description.clone();
                }
            }
        }
        self
    }
}

// ── Parser ──────────────────────────────────────────────────────────────────

/// Parse an x402 challenge from response headers and/or body, preferring a
/// specific network. Returns `None` when no payment requirements are present.
pub fn parse_x402_challenge_for_network(
    headers: &[(String, String)],
    body: Option<&str>,
    preferred_network: Option<&str>,
) -> Option<PaymentRequirements> {
    if let Some(header) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(PAYMENT_REQUIRED_HEADER))
        && let Some(req) = parse_payment_required_header(&header.1, preferred_network)
    {
        return Some(req);
    }

    if let Some(header) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(X402_V1_PAYMENT_REQUIRED_HEADER))
        && let Ok(req) = serde_json::from_str::<PaymentRequirements>(&header.1)
    {
        return Some(req);
    }

    if let Some(body) = body
        && let Some(req) = parse_accepts_body(body, preferred_network)
    {
        return Some(req);
    }

    None
}

fn parse_payment_required_header(
    header: &str,
    preferred_network: Option<&str>,
) -> Option<PaymentRequirements> {
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, header).ok()?;
    let envelope: PaymentRequiredEnvelope =
        serde_json::from_slice::<PaymentRequiredEnvelope>(&decoded)
            .ok()?
            .with_resource_on_accepts();
    select_requirement(envelope.accepts, preferred_network)
}

fn parse_accepts_body(body: &str, preferred_network: Option<&str>) -> Option<PaymentRequirements> {
    let envelope: PaymentRequiredEnvelope = serde_json::from_str::<PaymentRequiredEnvelope>(body)
        .ok()?
        .with_resource_on_accepts();
    select_requirement(envelope.accepts, preferred_network)
}

fn select_requirement(
    accepts: Vec<PaymentRequirements>,
    preferred_network: Option<&str>,
) -> Option<PaymentRequirements> {
    let preferred = preferred_network
        .map(caip2_network_for_cluster)
        .unwrap_or(SOLANA_MAINNET);

    fn amount(requirement: &PaymentRequirements) -> u64 {
        requirement.amount.parse::<u64>().unwrap_or(u64::MAX)
    }

    fn network_matches(requirement: &PaymentRequirements, preferred: &str) -> bool {
        requirement.network == preferred
            || (preferred == SOLANA_MAINNET && requirement.network == SOLANA_NETWORK)
            || requirement
                .cluster
                .as_deref()
                .map(caip2_network_for_cluster)
                .is_some_and(|network| network == preferred)
    }

    let mut filtered: Vec<PaymentRequirements> = accepts
        .into_iter()
        .filter(|r| network_matches(r, preferred))
        .collect();
    if filtered.is_empty() {
        return None;
    }
    filtered.sort_by_key(amount);
    filtered.into_iter().next()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Normalize a network identifier into canonical CAIP-2 form. Solana shorthand
/// (`"mainnet"`, `"devnet"`, etc.) maps to the corresponding CAIP-2 string;
/// everything else passes through unchanged.
pub fn normalize_network_identifier(network: &str) -> String {
    match network {
        SOLANA_NETWORK | "mainnet" | "mainnet-beta" => SOLANA_MAINNET.to_string(),
        "solana-devnet" | "devnet" | "localnet" => SOLANA_DEVNET.to_string(),
        "solana-testnet" | "testnet" => SOLANA_TESTNET.to_string(),
        value if value.starts_with("solana:") => value.to_string(),
        value => value.to_string(),
    }
}

/// Map a Solana cluster name (`"mainnet-beta"`) to its canonical CAIP-2 form.
pub fn caip2_network_for_cluster(cluster: &str) -> &'static str {
    match cluster {
        SOLANA_MAINNET | SOLANA_NETWORK | "mainnet" | "mainnet-beta" => SOLANA_MAINNET,
        SOLANA_TESTNET | "testnet" | "solana-testnet" => SOLANA_TESTNET,
        "devnet" | "localnet" => SOLANA_DEVNET,
        SOLANA_DEVNET | "solana-devnet" => SOLANA_DEVNET,
        _ => SOLANA_MAINNET,
    }
}

/// Reverse of [`caip2_network_for_cluster`] for the three well-known Solana
/// CAIP-2 chains. Returns `None` for non-Solana CAIP-2 identifiers.
pub fn cluster_for_caip2_network(caip2: &str) -> Option<&'static str> {
    match caip2 {
        SOLANA_MAINNET => Some("mainnet-beta"),
        SOLANA_DEVNET => Some("devnet"),
        SOLANA_TESTNET => Some("testnet"),
        _ => None,
    }
}

fn string_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn u64_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(|value| value.as_u64())
}

fn u8_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u8> {
    u64_field(object, key).and_then(|value| u8::try_from(value).ok())
}

fn bool_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<bool> {
    object.get(key).and_then(|value| value.as_bool())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_constants_match_protocol() {
        assert_eq!(X402_VERSION_V1, 1);
        assert_eq!(X402_VERSION_V2, 2);
    }

    #[test]
    fn header_constants_v2_canonical() {
        assert_eq!(PAYMENT_REQUIRED_HEADER, "PAYMENT-REQUIRED");
        assert_eq!(PAYMENT_SIGNATURE_HEADER, "PAYMENT-SIGNATURE");
        assert_eq!(PAYMENT_RESPONSE_HEADER, "PAYMENT-RESPONSE");
    }

    #[test]
    fn normalize_network_solana_shorthands() {
        assert_eq!(normalize_network_identifier("mainnet"), SOLANA_MAINNET);
        assert_eq!(normalize_network_identifier("devnet"), SOLANA_DEVNET);
        assert_eq!(normalize_network_identifier("eip155:8453"), "eip155:8453");
    }

    #[test]
    fn deser_roundtrip_v2_evm_shape() {
        let input = serde_json::json!({
            "scheme": "exact",
            "network": "eip155:8453",
            "amount": "10000",
            "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "payTo": "0x1234567890123456789012345678901234567890",
            "maxTimeoutSeconds": 300,
        });
        let req: PaymentRequirements = serde_json::from_value(input).unwrap();
        assert_eq!(req.network, "eip155:8453");
        assert_eq!(req.recipient, "0x1234567890123456789012345678901234567890");
        assert_eq!(req.amount, "10000");
        assert_eq!(req.currency, "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        assert_eq!(req.max_age, Some(300));
    }

    #[test]
    fn deser_envelope_with_resource_propagation() {
        let body = serde_json::json!({
            "x402Version": 2,
            "resource": { "url": "https://api.example/foo" },
            "accepts": [{
                "scheme": "exact",
                "network": "eip155:8453",
                "amount": "10000",
                "asset": "0xabc",
                "payTo": "0xrecipient",
                "maxTimeoutSeconds": 300,
            }],
        });
        let env: PaymentRequiredEnvelope = serde_json::from_value(body).unwrap();
        let env = env.with_resource_on_accepts();
        assert_eq!(env.accepts[0].resource, "https://api.example/foo");
    }
}
