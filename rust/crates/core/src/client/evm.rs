//! EVM x402 payment builder.
//!
//! Wraps the official Coinbase `x402-chain-eip155` V2 client so the multi-chain
//! `client::x402::build_payment` dispatch can produce an EIP-712 / ERC-3009
//! `PAYMENT-SIGNATURE` header for `eip155:*` networks. The module is only
//! compiled under the `evm` Cargo feature; the Solana payment path never
//! touches it.

use solana_x402::exact::PaymentRequirements;
use tracing::info;
use x402_chain_eip155::V2Eip155ExactClient;
use x402_types::proto::v2;
use x402_types::proto::{self, OriginalJson};
use x402_types::scheme::client::X402SchemeClient;

use crate::accounts::AccountsStore;
use crate::chain::{ChainFamily, ChainSigner};
use crate::client::x402::{BuiltPayment, Challenge, X402_V2_PAYMENT_HEADER};
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    let header_value = rt
        .block_on(sign_evm_payment(&evm_signer, requirements))
        .map_err(|e| Error::Mpp(format!("Failed to build EVM x402 payment: {e}")))?;

    info!(
        network = %network,
        chain_id = %chain_id,
        address = %evm_signer.address(),
        amount = %requirements.amount,
        currency = %requirements.currency,
        "Built EVM x402 payment"
    );

    Ok(BuiltPayment {
        headers: vec![(X402_V2_PAYMENT_HEADER, header_value)],
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

/// Drive `V2Eip155ExactClient` with a single-entry `PaymentRequired::V2`
/// envelope built from the chosen requirements, then sign the first
/// candidate it accepts. The b64-encoded payload is the
/// `PAYMENT-SIGNATURE` header value.
async fn sign_evm_payment(
    signer: &crate::chain::EvmChainSigner,
    requirements: &PaymentRequirements,
) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let accepted_json = requirements.to_accepted_value();
    let raw = serde_json::value::RawValue::from_string(accepted_json.to_string())?;
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
            secret_key_hex: Some(signer.to_hex_key()),
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
}
