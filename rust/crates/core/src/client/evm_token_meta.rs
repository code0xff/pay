//! On-chain ERC-20 + EIP-712 metadata cache for EVM tokens.
//!
//! Phase 13 replaces Phase 12's static `evm_stablecoin_decimals(symbol)`
//! mapping with an authoritative on-chain lookup. The same RPC call also
//! fetches the EIP-712 `(name, version)` domain hint (EIP-5267), so the
//! x402 server envelope no longer needs the hardcoded
//! `usdc_eip712_domain(slug)` table — the token contract itself becomes
//! the source of truth.
//!
//! Lookups are cached per `(chain_id, token_address)` in an `OnceLock`-
//! initialized `tokio::sync::RwLock<HashMap<...>>`. The first paid request
//! per token pays one extra `eth_call` round trip; every subsequent
//! request reads from the cache.


use std::collections::HashMap;
use std::sync::OnceLock;

use alloy::primitives::Address;
use tokio::sync::RwLock;

use crate::client::balance::evm_stablecoin_decimals;

/// Token metadata used to build / verify x402 envelopes.
#[derive(Clone, Debug)]
pub struct EvmTokenMeta {
    /// Result of the ERC-20 `decimals()` view, or the symbol-based
    /// fallback if the RPC call failed.
    pub decimals: u8,
    /// EIP-712 domain hint emitted in `extra`. Comes from EIP-5267
    /// `eip712Domain()` when available, otherwise a chain-derived guess.
    pub eip712_domain: Eip712Domain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip712Domain {
    pub name: String,
    pub version: String,
}

alloy::sol! {
    #[sol(rpc)]
    interface IErc20Meta {
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    interface IEip5267 {
        function eip712Domain() external view returns (
            bytes1 fields,
            string memory name,
            string memory version,
            uint256 chainId,
            address verifyingContract,
            bytes32 salt,
            uint256[] memory extensions
        );
    }
}

type CacheMap = HashMap<(u64, Address), EvmTokenMeta>;
static CACHE: OnceLock<RwLock<CacheMap>> = OnceLock::new();

fn cache() -> &'static RwLock<CacheMap> {
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Look up `EvmTokenMeta` for `(chain_id, token)`. On a cache miss this
/// issues two view calls in sequence — `decimals()` first because it's
/// universally implemented, then `eip712Domain()` (EIP-5267) which some
/// older deployments are missing. Both calls fall back gracefully:
///   - missing `decimals()` → symbol-based fallback via `fallback_symbol`
///   - missing `eip712Domain()` → `static_fallback_domain(chain_id)`
///
/// This keeps the gateway from 500-ing on a marginal token that doesn't
/// implement EIP-5267 yet.
pub async fn fetch_token_meta(
    rpc_url: &str,
    chain_id: u64,
    token: Address,
    fallback_symbol: Option<&str>,
) -> Result<EvmTokenMeta, String> {
    if let Some(hit) = cache().read().await.get(&(chain_id, token)) {
        return Ok(hit.clone());
    }

    let parsed_url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| format!("invalid EVM RPC URL `{rpc_url}`: {e}"))?;
    let provider = alloy::providers::ProviderBuilder::new().connect_http(parsed_url);

    let decimals = match IErc20Meta::new(token, &provider).decimals().call().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                token = %token,
                chain_id,
                error = %e,
                "decimals() RPC failed — falling back to symbol-based default"
            );
            fallback_symbol
                .and_then(evm_stablecoin_decimals)
                .ok_or_else(|| {
                    format!(
                        "decimals() RPC failed and no fallback symbol registered for {token} on chain {chain_id}: {e}"
                    )
                })?
        }
    };

    let domain = match IEip5267::new(token, &provider).eip712Domain().call().await {
        Ok(d) => Eip712Domain {
            name: d.name.to_string(),
            version: d.version.to_string(),
        },
        Err(e) => {
            tracing::warn!(
                token = %token,
                chain_id,
                error = %e,
                "eip712Domain() RPC failed — falling back to chain-derived domain"
            );
            static_fallback_domain(chain_id)
        }
    };

    let meta = EvmTokenMeta {
        decimals,
        eip712_domain: domain,
    };
    cache()
        .write()
        .await
        .insert((chain_id, token), meta.clone());
    Ok(meta)
}

/// Best-effort fallback `(name, version)` when the contract doesn't
/// implement EIP-5267. Mirrors the values `x402-chain-eip155` ships for
/// USDC on the listed networks so the facilitator still accepts the
/// envelope when our pre-flight metadata fetch fails.
pub(crate) fn static_fallback_domain(chain_id: u64) -> Eip712Domain {
    let (name, version) = match chain_id {
        // Ethereum mainnet, Base, Optimism, Arbitrum native USDC.
        1 | 8453 | 10 | 42161 => ("USD Coin", "2"),
        // Sepolia / Holesky / Base-Sepolia USDC test deployments.
        11155111 | 17000 | 84532 => ("USDC", "2"),
        _ => ("USDC", "2"),
    };
    Eip712Domain {
        name: name.to_string(),
        version: version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_fallback_picks_long_name_for_mainnet_l2s() {
        let d = static_fallback_domain(8453);
        assert_eq!(d.name, "USD Coin");
        assert_eq!(d.version, "2");
    }

    #[test]
    fn static_fallback_picks_short_name_for_testnets() {
        let d = static_fallback_domain(11155111);
        assert_eq!(d.name, "USDC");
        let d = static_fallback_domain(84532);
        assert_eq!(d.name, "USDC");
    }

    #[test]
    fn static_fallback_picks_short_name_for_unknown_chain() {
        // Unknown chain ids land on the conservative short form rather
        // than panicking, so a new L2 keeps working until we wire its
        // native USDC into the table.
        let d = static_fallback_domain(987654321);
        assert_eq!(d.name, "USDC");
    }
}
