//! EVM x402 payment builder.
//!
//! Wraps the official Coinbase `x402-chain-eip155` V2 client so the multi-chain
//! `client::x402::build_payment` dispatch can produce an EIP-712 / ERC-3009
//! `PAYMENT-SIGNATURE` header for `eip155:*` networks. The module is only
//! compiled under the `evm` Cargo feature; the Solana payment path never
//! touches it.
#![cfg(feature = "evm")]

use solana_x402::exact::PaymentRequirements;
use solana_x402::{X402_VERSION_V1, X402_VERSION_V2};
use tracing::info;
use x402_chain_eip155::{V1Eip155ExactClient, V2Eip155ExactClient};
use x402_types::proto::{self, OriginalJson, v1, v2};
use x402_types::scheme::client::X402SchemeClient;

use crate::accounts::AccountsStore;
use crate::chain::{ChainFamily, ChainSigner};
use crate::client::x402::{
    BuiltPayment, Challenge, X402_V1_PAYMENT_HEADER, X402_V2_PAYMENT_HEADER,
};
use crate::signer::load_evm_signer_for_network;
use crate::{Error, Result};

/// Build an EVM x402 payment header.
///
/// `network` is the pay-side slug (e.g. `"base"`, `"sepolia"`); `requirements`
/// is the `accepts` entry already chosen by `client::x402::select_best_chain`.
pub fn build_evm_payment(
    challenge: &Challenge,
    requirements: &PaymentRequirements,
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
) -> Result<BuiltPayment> {
    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => {
            return Err(Error::Config(format!(
                "Network `{network}` is not an EVM network"
            )));
        }
    };

    let (evm_signer, ephemeral_notice) =
        load_evm_signer_for_network(network, store, account_override)?;

    let _ = challenge; // resource info already lives inside requirements

    // Phase 14: dispatch on x402 protocol version. v2 is the modern
    // default (`PAYMENT-SIGNATURE` header, v2 envelope); v1 is still
    // observed in the wild on Ethereum-family servers that haven't
    // migrated. Both versions share the same ERC-3009 signing primitive
    // under x402-chain-eip155.
    let version = challenge.x402_version;
    let header_name = match version {
        X402_VERSION_V1 => X402_V1_PAYMENT_HEADER,
        X402_VERSION_V2 => X402_V2_PAYMENT_HEADER,
        other => {
            return Err(Error::Config(format!(
                "EVM x402 payment builder does not support x402_version `{other}`"
            )));
        }
    };

    // EIP-712 / ERC-3009 signing is purely local (no network I/O), so a
    // lightweight current-thread runtime is sufficient. Avoids spawning a
    // fresh multi-thread worker pool on every 402 retry, which under
    // concurrent load would exhaust OS thread limits.
    let header_value = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(sign_evm_payment(&evm_signer, requirements, version))
        }),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?
            .block_on(sign_evm_payment(&evm_signer, requirements, version)),
    }
    .map_err(|e| Error::Mpp(format!("Failed to build EVM x402 payment: {e}")))?;

    info!(
        network = %network,
        chain_id = %chain_id,
        x402_version = %version,
        address = %evm_signer.address(),
        amount = %requirements.amount,
        currency = %requirements.currency,
        "Built EVM x402 payment"
    );

    Ok(BuiltPayment {
        headers: vec![(header_name, header_value)],
        ephemeral_notice,
    })
}

/// Pick the best candidate from what `V2Eip155ExactClient::accept` returned.
///
/// Phase 13-6: makes candidate selection an explicit, named step instead of
/// an inline `.next()`. Currently selects the first candidate (the SDK
/// already scores and orders them), leaving a clear hook for future
/// preferred-symbol or balance-based selection without touching the call site.
fn pick_best_candidate<C>(
    candidates: impl IntoIterator<Item = C>,
    network: &str,
) -> std::result::Result<C, Box<dyn std::error::Error + Send + Sync>> {
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            Box::from(format!(
                "x402-chain-eip155 produced no candidate for network `{network}` \
             (requirements may have an unsupported `extra` shape)"
            ))
        })
}

/// Reshape a v2-style `to_accepted_value()` JSON object into the v1
/// `PaymentRequirements` shape: rewrite CAIP-2 network to the short
/// name the upstream SDK's `ChainId::from_network_name` recognizes,
/// rename `amount` -> `maxAmountRequired`, and add the
/// `resource`/`description` fields v1 requires but v2 emits separately
/// (sourced from the original `PaymentRequirements`). Returns `None`
/// when the input is not a JSON object so callers can pass the
/// original payload through unchanged.
fn v1_envelope_reshape(
    accepted_json: &serde_json::Value,
    req: &PaymentRequirements,
) -> Option<String> {
    let mut rewritten = accepted_json.clone();
    let map = rewritten.as_object_mut()?;
    if let Some(chain_id_str) = req.network.strip_prefix("eip155:") {
        let chain_id =
            x402_types::chain::ChainId::new("eip155".to_string(), chain_id_str.to_string());
        if let Some(short) = chain_id.as_network_name() {
            map.insert(
                "network".to_string(),
                serde_json::Value::String(short.to_string()),
            );
        }
    }
    // v1 calls the field `maxAmountRequired`; v2 ships only `amount`.
    if let Some(amount) = map.remove("amount") {
        map.insert("maxAmountRequired".to_string(), amount);
    }
    // v1 requires resource + description on every accepts entry.
    map.entry("resource".to_string())
        .or_insert_with(|| serde_json::Value::String(req.resource.clone()));
    map.entry("description".to_string()).or_insert_with(|| {
        serde_json::Value::String(req.description.clone().unwrap_or_default())
    });
    Some(rewritten.to_string())
}

/// Drive the appropriate `*Eip155ExactClient` with a single-entry
/// `PaymentRequired` envelope built from the chosen requirements, then
/// sign the first candidate it accepts. The b64-encoded payload becomes
/// the value of `PAYMENT-SIGNATURE` (v2) or `X-Payment` (v1).
async fn sign_evm_payment(
    signer: &crate::chain::EvmChainSigner,
    requirements: &PaymentRequirements,
    version: u64,
) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let accepted_json = requirements.to_accepted_value();
    let raw = serde_json::value::RawValue::from_string(accepted_json.to_string())?;

    match version {
        X402_VERSION_V1 => {
            // x402 v1 PaymentRequirements differs from v2 in field set
            // and naming: it uses short network names (no CAIP-2),
            // `maxAmountRequired` instead of `amount`, and requires
            // `resource`/`description`/`maxTimeoutSeconds`. v2's
            // `to_accepted_value` omits some of these, so reshape the
            // envelope before handing it to the v1 client.
            let raw = match v1_envelope_reshape(&accepted_json, requirements) {
                Some(rewritten) => serde_json::value::RawValue::from_string(rewritten)?,
                None => raw,
            };
            let envelope = v1::PaymentRequired::<OriginalJson> {
                x402_version: v1::X402Version1,
                accepts: vec![OriginalJson(raw)],
                error: None,
            };
            let payment_required = proto::PaymentRequired::V1(envelope);
            let client = V1Eip155ExactClient::new(signer.signer.clone());
            let candidates = client.accept(&payment_required);
            let candidate = pick_best_candidate(candidates, &requirements.network)?;
            Ok(candidate.sign().await?)
        }
        X402_VERSION_V2 => {
            let envelope = v2::PaymentRequired::<OriginalJson> {
                x402_version: v2::X402Version2,
                error: None,
                resource: None,
                accepts: vec![OriginalJson(raw)],
            };
            let payment_required = proto::PaymentRequired::V2(envelope);
            let client = V2Eip155ExactClient::new(signer.signer.clone());
            let candidates = client.accept(&payment_required);
            let candidate = pick_best_candidate(candidates, &requirements.network)?;
            Ok(candidate.sign().await?)
        }
        other => Err(format!("unsupported x402_version {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, AccountsFile, Keystore, MemoryAccountsStore};
    use crate::chain::EvmChainSigner;
    use solana_x402::X402_VERSION_V2;
    use solana_x402::exact::PaymentRequirements;

    fn sepolia_account(signer: &EvmChainSigner) -> Account {
        Account {
            keystore: Keystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: Some(signer.address()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: None,
            chain_family: Some("evm".to_string()),
            secret_key_hex: Some(signer.to_hex_key().to_string()),
            created_at: Some("2026-05-12T00:00:00Z".to_string()),
        }
    }

    fn evm_sepolia_requirements() -> PaymentRequirements {
        PaymentRequirements {
            network: "eip155:11155111".to_string(),
            cluster: None,
            recipient: "0x000000000000000000000000000000000000dEaD".to_string(),
            amount: "1000000".to_string(),
            // ERC-3009 expects the asset address; USDC on Sepolia.
            currency: "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238".to_string(),
            decimals: Some(6),
            token_program: None,
            resource: "https://example.com/resource".to_string(),
            description: Some("EVM smoke test".to_string()),
            max_age: Some(300),
            recent_blockhash: None,
            fee_payer: None,
            fee_payer_key: None,
            extra: Some(serde_json::json!({
                "name": "USDC",
                "version": "2",
            })),
            accepted: None,
            resource_info: None,
        }
    }

    #[test]
    fn build_evm_payment_produces_payment_signature_header() {
        let signer = EvmChainSigner::random(11_155_111);
        let mut file = AccountsFile::default();
        file.upsert("sepolia", "default", sepolia_account(&signer));
        let store = MemoryAccountsStore::with_file(file);

        let requirements = evm_sepolia_requirements();
        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements: requirements.clone(),
            all_accepts: vec![requirements.clone()],
            siwx: None,
        };

        let built = build_evm_payment(&challenge, &requirements, "sepolia", &store, None)
            .expect("EVM payment build");

        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].0, X402_V2_PAYMENT_HEADER);
        assert!(!built.headers[0].1.is_empty());
    }

    #[test]
    fn build_evm_payment_rejects_non_evm_network() {
        let store = MemoryAccountsStore::new();
        let requirements = evm_sepolia_requirements();
        let challenge = Challenge {
            x402_version: X402_VERSION_V2,
            requirements: requirements.clone(),
            all_accepts: vec![requirements.clone()],
            siwx: None,
        };

        let err =
            build_evm_payment(&challenge, &requirements, "mainnet", &store, None).unwrap_err();
        assert!(
            err.to_string().contains("not an EVM network"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_evm_payment_v1_emits_x_payment_header() {
        // Phase 14: v1 envelopes use `X-Payment` instead of
        // `PAYMENT-SIGNATURE`. The same ERC-3009 signer drives both
        // versions via x402-chain-eip155's V1/V2 clients. Use base-sepolia
        // because the upstream x402-types short-name table doesn't
        // include Ethereum Sepolia.
        let signer = EvmChainSigner::random(84_532);
        let mut file = AccountsFile::default();
        file.upsert("base-sepolia", "default", sepolia_account(&signer));
        let store = MemoryAccountsStore::with_file(file);

        let mut requirements = evm_sepolia_requirements();
        requirements.network = "eip155:84532".to_string();
        requirements.currency = "0x036CbD53842c5426634e7929541eC2318f3dCF7e".to_string();
        let challenge = Challenge {
            x402_version: solana_x402::X402_VERSION_V1,
            requirements: requirements.clone(),
            all_accepts: vec![requirements.clone()],
            siwx: None,
        };

        let built = build_evm_payment(&challenge, &requirements, "base-sepolia", &store, None)
            .expect("v1 EVM payment build");
        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].0, X402_V1_PAYMENT_HEADER);
        assert!(!built.headers[0].1.is_empty());
    }

    #[test]
    fn build_evm_payment_rejects_unknown_x402_version() {
        let signer = EvmChainSigner::random(11_155_111);
        let mut file = AccountsFile::default();
        file.upsert("sepolia", "default", sepolia_account(&signer));
        let store = MemoryAccountsStore::with_file(file);

        let requirements = evm_sepolia_requirements();
        let challenge = Challenge {
            x402_version: 99,
            requirements: requirements.clone(),
            all_accepts: vec![requirements.clone()],
            siwx: None,
        };

        let err = build_evm_payment(&challenge, &requirements, "sepolia", &store, None)
            .unwrap_err();
        assert!(err.to_string().contains("x402_version"), "got: {err}");
    }
}
