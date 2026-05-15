//! Chain family abstraction — wraps ed25519 (Solana) and secp256k1 (EVM).
//!
//! New protocols plug in by implementing `ChainSigner`.
//! Existing Solana code continues to use `MemorySigner` directly; this
//! module is only used by the x402 multi-chain dispatch path.
//!
//! The entire module is gated behind the `evm` Cargo feature; the
//! Solana-only build does not include any of these types.

use crate::{Error, Result};
// Brings `with_chain_id` into scope for `PrivateKeySigner`.
use alloy::signers::Signer;

// ── ChainFamily ──────────────────────────────────────────────────────────────

/// Identifies the on-chain ecosystem for a given account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainFamily {
    Solana,
    Evm { chain_id: u64 },
}

impl ChainFamily {
    /// Parse a network slug (accounts.yml key) into a ChainFamily.
    pub fn from_network_slug(slug: &str) -> Self {
        match slug {
            "mainnet" | "devnet" | "testnet" | "localnet" => ChainFamily::Solana,
            "ethereum" => ChainFamily::Evm { chain_id: 1 },
            "base" => ChainFamily::Evm { chain_id: 8453 },
            "optimism" => ChainFamily::Evm { chain_id: 10 },
            "arbitrum" => ChainFamily::Evm { chain_id: 42161 },
            "sepolia" => ChainFamily::Evm { chain_id: 11155111 },
            "holesky" => ChainFamily::Evm { chain_id: 17000 },
            "base-sepolia" => ChainFamily::Evm { chain_id: 84532 },
            other => {
                if let Some(id_str) = other.strip_prefix("eip155:")
                    && let Ok(id) = id_str.parse::<u64>()
                {
                    return ChainFamily::Evm { chain_id: id };
                }
                ChainFamily::Solana
            }
        }
    }

    /// Parse a CAIP-2 chain identifier into a ChainFamily.
    pub fn from_caip2(caip2: &str) -> Option<Self> {
        if caip2.starts_with("solana:") {
            return Some(ChainFamily::Solana);
        }
        if let Some(id_str) = caip2.strip_prefix("eip155:") {
            let chain_id = id_str.parse::<u64>().ok()?;
            return Some(ChainFamily::Evm { chain_id });
        }
        None
    }

    /// Convert to a pay-internal network slug.
    pub fn to_network_slug(&self) -> &'static str {
        match self {
            ChainFamily::Solana => "mainnet",
            ChainFamily::Evm { chain_id: 1 } => "ethereum",
            ChainFamily::Evm { chain_id: 8453 } => "base",
            ChainFamily::Evm { chain_id: 10 } => "optimism",
            ChainFamily::Evm { chain_id: 42161 } => "arbitrum",
            ChainFamily::Evm { chain_id: 11155111 } => "sepolia",
            ChainFamily::Evm { chain_id: 17000 } => "holesky",
            ChainFamily::Evm { chain_id: 84532 } => "base-sepolia",
            ChainFamily::Evm { .. } => "evm-unknown",
        }
    }

    pub fn is_evm(&self) -> bool {
        matches!(self, ChainFamily::Evm { .. })
    }

    pub fn is_solana(&self) -> bool {
        matches!(self, ChainFamily::Solana)
    }
}

// ── ChainSigner trait ────────────────────────────────────────────────────────

/// Unified signing interface used by the x402 multi-chain dispatch path.
///
/// Today this is implemented only for EVM (`EvmChainSigner`). The Solana
/// payment path in x402 reuses the existing `solana-x402` SDK directly,
/// so wrapping `MemorySigner` here would be unused indirection. If a
/// Solana implementation is needed later, add it then — `solana_keychain::
/// SolanaSigner` is async-only so the trait method here would need to
/// become `async fn` (or block on a runtime).
pub trait ChainSigner: Send + Sync {
    /// Sign a raw message (already hashed if needed by the scheme).
    fn sign_raw(&self, message: &[u8]) -> Vec<u8>;

    /// Public address — 0x-hex for EVM.
    fn address(&self) -> String;

    /// Which chain this signer operates on.
    fn chain_family(&self) -> ChainFamily;
}

// ── EvmChainSigner ───────────────────────────────────────────────────────────

/// Wraps alloy `PrivateKeySigner` behind `ChainSigner`.
pub struct EvmChainSigner {
    pub signer: alloy::signers::local::PrivateKeySigner,
    pub chain_id: u64,
}

impl EvmChainSigner {
    /// Create from a 32-byte hex private key (with or without "0x" prefix).
    pub fn from_hex(hex_key: &str, chain_id: u64) -> Result<Self> {
        let key = hex_key.strip_prefix("0x").unwrap_or(hex_key);
        let signer = key
            .parse::<alloy::signers::local::PrivateKeySigner>()
            .map_err(|e| Error::Config(format!("Invalid EVM private key: {e}")))?;
        let signer = signer.with_chain_id(Some(chain_id));
        Ok(Self { signer, chain_id })
    }

    /// Generate a fresh random ephemeral signer.
    pub fn random(chain_id: u64) -> Self {
        let signer =
            alloy::signers::local::PrivateKeySigner::random().with_chain_id(Some(chain_id));
        Self { signer, chain_id }
    }

    /// Export 32-byte private key as lowercase hex (no 0x prefix).
    pub fn to_hex_key(&self) -> String {
        hex::encode(self.signer.credential().to_bytes())
    }

    /// Export the raw 32-byte secp256k1 private key. Used when persisting to
    /// an OS keystore so callers don't have to re-decode `to_hex_key()`.
    pub fn to_private_key_bytes(&self) -> [u8; 32] {
        self.signer.credential().to_bytes().into()
    }
}

impl ChainSigner for EvmChainSigner {
    fn sign_raw(&self, message: &[u8]) -> Vec<u8> {
        use alloy::signers::SignerSync;
        self.signer
            .sign_message_sync(message)
            .map(|sig| sig.as_bytes().to_vec())
            .unwrap_or_default()
    }

    fn address(&self) -> String {
        format!("{:?}", self.signer.address())
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm {
            chain_id: self.chain_id,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_family_from_slug() {
        assert_eq!(
            ChainFamily::from_network_slug("mainnet"),
            ChainFamily::Solana
        );
        assert_eq!(
            ChainFamily::from_network_slug("ethereum"),
            ChainFamily::Evm { chain_id: 1 }
        );
        assert_eq!(
            ChainFamily::from_network_slug("base"),
            ChainFamily::Evm { chain_id: 8453 }
        );
        assert_eq!(
            ChainFamily::from_network_slug("sepolia"),
            ChainFamily::Evm { chain_id: 11155111 }
        );
    }

    #[test]
    fn chain_family_from_caip2() {
        assert_eq!(
            ChainFamily::from_caip2("eip155:1"),
            Some(ChainFamily::Evm { chain_id: 1 })
        );
        assert_eq!(
            ChainFamily::from_caip2("eip155:8453"),
            Some(ChainFamily::Evm { chain_id: 8453 })
        );
        assert!(
            ChainFamily::from_caip2("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp")
                .unwrap()
                .is_solana()
        );
        assert_eq!(ChainFamily::from_caip2("unknown:123"), None);
    }

    #[test]
    fn evm_signer_random_has_valid_address() {
        let signer = EvmChainSigner::random(1);
        let addr = signer.address();
        assert!(
            addr.starts_with("0x"),
            "EVM address must start with 0x: {addr}"
        );
        assert_eq!(addr.len(), 42, "EVM address must be 42 chars: {addr}");
    }

    #[test]
    fn evm_signer_hex_roundtrip() {
        let signer1 = EvmChainSigner::random(1);
        let hex = signer1.to_hex_key();
        let signer2 = EvmChainSigner::from_hex(&hex, 1).unwrap();
        assert_eq!(signer1.address(), signer2.address());
    }

    #[test]
    fn chain_family_is_evm_and_is_solana() {
        assert!(ChainFamily::Solana.is_solana());
        assert!(!ChainFamily::Solana.is_evm());
        assert!(ChainFamily::Evm { chain_id: 1 }.is_evm());
        assert!(!ChainFamily::Evm { chain_id: 1 }.is_solana());
    }
}
