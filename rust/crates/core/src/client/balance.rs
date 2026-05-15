//! Wallet balance lookups.
//!
//! - SOL is fetched directly from a Solana JSON-RPC endpoint (`getBalance` /
//!   `getMultipleAccounts`).
//! - Token balances come from the **pay-api** stablecoin service
//!   (`GET /v1/balance/stablecoins`). pay-api derives ATAs locally and does a
//!   single `getMultipleAccounts` call against its own configured RPC, so we
//!   pay one HTTP round trip here rather than scanning every token account.
//!
//! Environment variables:
//! - `PAY_MAINNET_RPC_URL` — override the default Solana mainnet RPC.
//! - `PAY_API_URL`         — override the pay-api host (default [`DEFAULT_PAY_API_URL`]).

use pay_types::Stablecoin;
use serde::Deserialize;
use std::collections::HashMap;

/// Default pay-api host. Override with `PAY_API_URL`.
pub const DEFAULT_PAY_API_URL: &str = "https://api.gateway-402.com";

/// Default mainnet RPC URL. Override with `PAY_MAINNET_RPC_URL`.
pub fn mainnet_rpc_url() -> String {
    std::env::var("PAY_MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string())
}

/// Default pay-api host. Override with `PAY_API_URL`.
pub fn pay_api_url() -> String {
    std::env::var("PAY_API_URL").unwrap_or_else(|_| DEFAULT_PAY_API_URL.to_string())
}

fn mint_symbol(mint: &str) -> Option<&'static str> {
    Stablecoin::symbol_for_mint(mint)
}

/// Map an RPC URL to the network name pay-api expects.
fn infer_network(rpc_url: &str) -> &'static str {
    let lower = rpc_url.to_lowercase();
    if lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("surfnet")
        || lower.contains("surfpool")
    {
        "sandbox"
    } else {
        "mainnet"
    }
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub mint: String,
    pub raw_amount: u64,
    pub ui_amount: f64,
    pub symbol: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct AccountBalances {
    pub sol_lamports: u64,
    pub tokens: Vec<TokenBalance>,
    /// True when the pay-api stablecoin lookup failed for this account (e.g.
    /// pay-api unreachable). `tokens` will be empty in that case; callers
    /// should render an "unavailable" indicator instead of treating the
    /// account as zero-balance.
    pub tokens_unavailable: bool,
}

impl AccountBalances {
    pub fn diff_received(&self, baseline: &AccountBalances) -> ReceivedFunds {
        let sol_gained = self.sol_lamports.saturating_sub(baseline.sol_lamports);
        // If either side could not reach pay-api, the token list on that side
        // is missing rather than zero — diffing it would falsely report the
        // entire other side as "received". Skip token diff in that case; SOL
        // is still safe because it comes from RPC.
        let mut tokens = Vec::new();
        if !self.tokens_unavailable && !baseline.tokens_unavailable {
            for current in &self.tokens {
                let prev = baseline
                    .tokens
                    .iter()
                    .find(|t| t.mint == current.mint)
                    .map(|t| t.ui_amount)
                    .unwrap_or(0.0);
                let gained = current.ui_amount - prev;
                if gained > f64::EPSILON {
                    tokens.push(ReceivedToken {
                        mint: current.mint.clone(),
                        ui_amount: gained,
                        symbol: current.symbol,
                    });
                }
            }
        }
        ReceivedFunds {
            sol_lamports: sol_gained,
            tokens,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedFunds {
    pub sol_lamports: u64,
    pub tokens: Vec<ReceivedToken>,
}

#[derive(Debug, Clone)]
pub struct ReceivedToken {
    pub mint: String,
    pub ui_amount: f64,
    pub symbol: Option<&'static str>,
}

impl ReceivedFunds {
    pub fn has_any(&self) -> bool {
        self.sol_lamports > 0 || !self.tokens.is_empty()
    }
}

// ── pay-api wire types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse {
    balances: Vec<ApiBalance>,
}

#[derive(Deserialize)]
struct ApiBalance {
    mint: String,
    raw_amount: String,
    ui_amount: f64,
    // symbol & decimals are also returned but we recompute the symbol locally
    // so the rest of pay keeps a stable `Option<&'static str>`.
}

async fn fetch_stablecoins_via_api(
    client: &reqwest::Client,
    api_url: &str,
    pubkey: &str,
    network: &str,
) -> crate::Result<Vec<TokenBalance>> {
    let url = format!(
        "{}/v1/balance/stablecoins?address={}&network={}",
        api_url.trim_end_matches('/'),
        pubkey,
        network,
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| crate::Error::Config(format!("pay-api request error: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::Error::Config(format!(
            "pay-api returned HTTP {status}: {body}"
        )));
    }

    let parsed: ApiResponse = resp
        .json()
        .await
        .map_err(|e| crate::Error::Config(format!("pay-api decode error: {e}")))?;

    Ok(parsed
        .balances
        .into_iter()
        .filter_map(|b| {
            let raw: u64 = b.raw_amount.parse().ok()?;
            // Match the previous behaviour: skip zero balances.
            if raw == 0 {
                return None;
            }
            let symbol = mint_symbol(&b.mint);
            Some(TokenBalance {
                mint: b.mint,
                raw_amount: raw,
                ui_amount: b.ui_amount,
                symbol,
            })
        })
        .collect())
}

// ── public API ──────────────────────────────────────────────────────────────

/// Fetch SOL (direct RPC) and stablecoin balances (via pay-api) for a single pubkey.
pub async fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    let client = balance_client()?;

    let sol_resp = rpc_call(
        &client,
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
    let sol_lamports = sol_resp["result"]["value"].as_u64().unwrap_or(0);

    let (tokens, tokens_unavailable) =
        match fetch_stablecoins_via_api(&client, &pay_api_url(), pubkey, infer_network(rpc_url))
            .await
        {
            Ok(t) => (t, false),
            Err(e) => {
                tracing::debug!(error = %e, "pay-api unreachable; returning empty token balances");
                (Vec::new(), true)
            }
        };

    Ok(AccountBalances {
        sol_lamports,
        tokens,
        tokens_unavailable,
    })
}

/// Fetch only stablecoin balances via pay-api.
///
/// This is used by top-up flows where SOL transfers are intentionally ignored
/// and startup should not pay for an extra direct Solana RPC round trip.
pub async fn get_stablecoin_balances(
    rpc_url: &str,
    pubkey: &str,
) -> crate::Result<AccountBalances> {
    let client = balance_client()?;
    let (tokens, tokens_unavailable) =
        match fetch_stablecoins_via_api(&client, &pay_api_url(), pubkey, infer_network(rpc_url))
            .await
        {
            Ok(t) => (t, false),
            Err(e) => {
                tracing::debug!(error = %e, "pay-api unreachable; returning empty token balances");
                (Vec::new(), true)
            }
        };

    Ok(AccountBalances {
        sol_lamports: 0,
        tokens,
        tokens_unavailable,
    })
}

fn balance_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| crate::Error::Config(e.to_string()))
}

/// Fetch SOL and stablecoin balances for multiple pubkeys efficiently.
///
/// SOL: one `getMultipleAccounts` call.
/// Tokens: one concurrent pay-api call per pubkey.
pub async fn get_balances_batch(
    rpc_url: &str,
    pubkeys: &[String],
) -> HashMap<String, AccountBalances> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }

    let client = match balance_client() {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    // Initialise every pubkey with zero balances so missing entries still surface.
    let mut balances: HashMap<String, AccountBalances> = pubkeys
        .iter()
        .map(|pk| (pk.clone(), AccountBalances::default()))
        .collect();

    // ── SOL: one getMultipleAccounts call ────────────────────────────────
    if let Ok(resp) = rpc_call(
        &client,
        rpc_url,
        "getMultipleAccounts",
        serde_json::json!([pubkeys, { "commitment": "confirmed" }]),
    )
    .await
        && let Some(accounts) = resp["result"]["value"].as_array()
    {
        for (pk, account) in pubkeys.iter().zip(accounts.iter()) {
            let lamports = account["lamports"].as_u64().unwrap_or(0);
            if let Some(entry) = balances.get_mut(pk) {
                entry.sol_lamports = lamports;
            }
        }
    }

    fetch_stablecoin_balances_batch_into(&client, rpc_url, pubkeys, &mut balances).await;

    balances
}

/// Fetch only stablecoin balances for multiple pubkeys.
///
/// This skips the direct Solana RPC `getMultipleAccounts` request and uses one
/// concurrent pay-api call per pubkey.
pub async fn get_stablecoin_balances_batch(
    rpc_url: &str,
    pubkeys: &[String],
) -> HashMap<String, AccountBalances> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }

    let client = match balance_client() {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut balances: HashMap<String, AccountBalances> = pubkeys
        .iter()
        .map(|pk| (pk.clone(), AccountBalances::default()))
        .collect();

    fetch_stablecoin_balances_batch_into(&client, rpc_url, pubkeys, &mut balances).await;

    balances
}

async fn fetch_stablecoin_balances_batch_into(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkeys: &[String],
    balances: &mut HashMap<String, AccountBalances>,
) {
    let api = pay_api_url();
    let network = infer_network(rpc_url);
    let mut set = tokio::task::JoinSet::new();
    for pk in pubkeys {
        let client = client.clone();
        let api = api.clone();
        let pk = pk.clone();
        set.spawn(async move {
            (
                pk.clone(),
                fetch_stablecoins_via_api(&client, &api, &pk, network).await,
            )
        });
    }

    while let Some(Ok((pk, result))) = set.join_next().await {
        match result {
            Ok(tokens) => {
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens = tokens;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, %pk, "pay-api token fetch failed");
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens_unavailable = true;
                }
            }
        }
    }
}

async fn rpc_call(
    client: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::Error::Config(format!("RPC error: {e}")))?;

    if resp.status() == 429 {
        return Err(crate::Error::Config("RPC rate limited (429)".to_string()));
    }

    let result: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| crate::Error::Config(format!("RPC parse error: {e}")))?;

    if let Some(err) = result.get("error") {
        return Err(crate::Error::Config(format!("RPC error: {err}")));
    }

    Ok(result)
}

// ── EVM balance lookups ─────────────────────────────────────────────────────
//
// Gated behind the `evm` Cargo feature. The Solana path above is untouched —
// EVM callers must dispatch to `get_evm_balances` explicitly with a network
// slug, since the RPC URL alone is not enough to distinguish chain families.

#[cfg(feature = "evm")]
pub use evm_balances::{
    evm_default_rpc_url, evm_rpc_url, evm_stablecoin_address, evm_stablecoin_decimals,
    get_evm_balances,
};

#[cfg(feature = "evm")]
mod evm_balances {
    use super::{AccountBalances, TokenBalance};
    use alloy::primitives::{Address, U256, utils::format_units};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::sol;
    use std::str::FromStr;

    sol! {
        #[sol(rpc)]
        interface IERC20 {
            function balanceOf(address account) external view returns (uint256);
        }
    }

    /// Default EVM JSON-RPC URLs by pay network slug.
    /// Override per-network with `PAY_<NETWORK>_RPC_URL`
    /// (e.g. `PAY_SEPOLIA_RPC_URL`, `PAY_BASE_SEPOLIA_RPC_URL`).
    pub fn evm_default_rpc_url(network: &str) -> &'static str {
        match network {
            "ethereum" => "https://ethereum.publicnode.com",
            "base" => "https://base.publicnode.com",
            "optimism" => "https://optimism.publicnode.com",
            "arbitrum" => "https://arbitrum-one.publicnode.com",
            "sepolia" => "https://ethereum-sepolia.publicnode.com",
            "holesky" => "https://ethereum-holesky.publicnode.com",
            "base-sepolia" => "https://base-sepolia.publicnode.com",
            _ => "https://ethereum.publicnode.com",
        }
    }

    pub fn evm_rpc_url(network: &str) -> String {
        let env_key = format!("PAY_{}_RPC_URL", network.to_uppercase().replace('-', "_"));
        std::env::var(&env_key).unwrap_or_else(|_| evm_default_rpc_url(network).to_string())
    }

    /// Decimal places for an ERC-20 stablecoin symbol. Centralized so the
    /// balance fetcher, the x402 server envelope builder, and the `pay send`
    /// EVM path all agree. Returns `None` for symbols we don't know — callers
    /// surface that as an explicit error rather than guessing.
    pub fn evm_stablecoin_decimals(symbol: &str) -> Option<u8> {
        match symbol {
            // USDC and USDT use 6 decimals on every chain we currently
            // advertise; DAI/USDS-style 18-decimal tokens land here when added.
            "USDC" | "USDT" => Some(6),
            "DAI" => Some(18),
            _ => None,
        }
    }

    /// Well-known ERC-20 stablecoin contract addresses per network.
    /// Returns `None` when we don't track the (network, symbol) pair — caller
    /// silently skips it rather than guessing at an address.
    pub fn evm_stablecoin_address(network: &str, symbol: &str) -> Option<&'static str> {
        match (network, symbol) {
            ("ethereum", "USDC") => Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            ("ethereum", "USDT") => Some("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
            ("base", "USDC") => Some("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
            ("optimism", "USDC") => Some("0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85"),
            ("arbitrum", "USDC") => Some("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"),
            ("sepolia", "USDC") => Some("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
            ("base-sepolia", "USDC") => Some("0x036CbD53842c5426634e7929541eC2318f3dCF7e"),
            _ => None,
        }
    }

    /// Fetch ETH + well-known ERC-20 stablecoin balances for an EVM address.
    ///
    /// The Solana-shaped `AccountBalances` struct is reused so existing
    /// renderers work without changes:
    /// - `sol_lamports` stays 0 (no Solana native balance on EVM).
    /// - Native ETH is emitted as a `TokenBalance { symbol: Some("ETH") }`.
    /// - Stablecoin balances are emitted as `TokenBalance` with `symbol`
    ///   set to the human-readable ticker, `mint` to the contract address.
    pub async fn get_evm_balances(network: &str, address: &str) -> crate::Result<AccountBalances> {
        let rpc_url = evm_rpc_url(network);
        let parsed_url: reqwest::Url = rpc_url
            .parse()
            .map_err(|e| crate::Error::Config(format!("Invalid EVM RPC URL `{rpc_url}`: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(parsed_url);

        let addr = Address::from_str(address)
            .map_err(|e| crate::Error::Config(format!("Invalid EVM address `{address}`: {e}")))?;

        let mut tokens = Vec::new();

        match provider.get_balance(addr).await {
            Ok(wei) => {
                if wei > U256::ZERO {
                    let ui = format_units(wei, "ether")
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    tokens.push(TokenBalance {
                        mint: "ETH".to_string(),
                        raw_amount: u256_saturate_u64(wei),
                        ui_amount: ui,
                        symbol: Some("ETH"),
                    });
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, network, "eth_getBalance failed");
                return Err(crate::Error::Config(format!(
                    "eth_getBalance for {address} on {network} failed: {e}"
                )));
            }
        }

        for symbol in ["USDC", "USDT"] {
            let Some(contract_addr) = evm_stablecoin_address(network, symbol) else {
                continue;
            };
            // `evm_stablecoin_decimals` is the single source of truth — if a
            // new symbol shows up in `evm_stablecoin_address` but is missing
            // here, the loop skips it instead of mis-formatting.
            let Some(decimals) = evm_stablecoin_decimals(symbol) else {
                continue;
            };
            let contract = Address::from_str(contract_addr).expect("static stablecoin address");
            let erc20 = IERC20::new(contract, &provider);
            match erc20.balanceOf(addr).call().await {
                Ok(raw) if raw > U256::ZERO => {
                    let ui = format_units(raw, decimals)
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    tokens.push(TokenBalance {
                        mint: contract_addr.to_string(),
                        raw_amount: u256_saturate_u64(raw),
                        ui_amount: ui,
                        symbol: Some(symbol),
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        symbol,
                        contract = contract_addr,
                        error = %e,
                        "ERC-20 balanceOf failed — skipping"
                    );
                }
            }
        }

        Ok(AccountBalances {
            sol_lamports: 0,
            tokens,
            tokens_unavailable: false,
        })
    }

    fn u256_saturate_u64(v: U256) -> u64 {
        if v <= U256::from(u64::MAX) {
            v.to::<u64>()
        } else {
            u64::MAX
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn evm_default_rpc_url_returns_known_endpoints() {
            assert!(evm_default_rpc_url("ethereum").contains("ethereum"));
            assert!(evm_default_rpc_url("base").contains("base"));
            assert!(evm_default_rpc_url("sepolia").contains("sepolia"));
            assert!(evm_default_rpc_url("base-sepolia").contains("base-sepolia"));
            // Unknown slug falls back to Ethereum mainnet rather than panicking.
            assert!(evm_default_rpc_url("polygon").contains("ethereum"));
        }

        #[test]
        fn evm_rpc_url_respects_env_override() {
            // SAFETY: single-threaded test context.
            unsafe { std::env::set_var("PAY_SEPOLIA_RPC_URL", "https://example.test/sepolia") };
            assert_eq!(evm_rpc_url("sepolia"), "https://example.test/sepolia");
            unsafe { std::env::remove_var("PAY_SEPOLIA_RPC_URL") };
            assert_eq!(evm_rpc_url("sepolia"), evm_default_rpc_url("sepolia"));
        }

        #[test]
        fn evm_rpc_url_normalises_hyphenated_slugs() {
            unsafe {
                std::env::set_var(
                    "PAY_BASE_SEPOLIA_RPC_URL",
                    "https://example.test/base-sepolia",
                )
            };
            assert_eq!(
                evm_rpc_url("base-sepolia"),
                "https://example.test/base-sepolia"
            );
            unsafe { std::env::remove_var("PAY_BASE_SEPOLIA_RPC_URL") };
        }

        #[test]
        fn evm_stablecoin_address_known_pairs() {
            assert!(evm_stablecoin_address("ethereum", "USDC").is_some());
            assert!(evm_stablecoin_address("ethereum", "USDT").is_some());
            assert!(evm_stablecoin_address("base", "USDC").is_some());
            assert!(evm_stablecoin_address("sepolia", "USDC").is_some());
            assert!(evm_stablecoin_address("base-sepolia", "USDC").is_some());
            // Unknown pair returns None rather than guessing.
            assert!(evm_stablecoin_address("polygon", "USDC").is_none());
            assert!(evm_stablecoin_address("base", "USDT").is_none());
        }

        #[test]
        fn u256_saturate_u64_clamps_oversized_values() {
            assert_eq!(u256_saturate_u64(U256::ZERO), 0);
            assert_eq!(u256_saturate_u64(U256::from(123u64)), 123);
            assert_eq!(u256_saturate_u64(U256::from(u64::MAX)), u64::MAX);
            let huge = U256::from(u64::MAX).checked_add(U256::from(1u64)).unwrap();
            assert_eq!(u256_saturate_u64(huge), u64::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_symbol_usdc() {
        assert_eq!(
            mint_symbol("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            Some("USDC")
        );
    }

    #[test]
    fn mint_symbol_usdt() {
        assert_eq!(
            mint_symbol("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
            Some("USDT")
        );
    }

    #[test]
    fn mint_symbol_usdg() {
        assert_eq!(
            mint_symbol("2u1tszSeqZ3qBWF3uNGPFc8TzMk2tdiwknnRMWGWjGWH"),
            Some("USDG")
        );
    }

    #[test]
    fn mint_symbol_unknown() {
        assert_eq!(
            mint_symbol("SomeRandomMint1111111111111111111111111111"),
            None
        );
    }

    #[test]
    fn mainnet_rpc_url_default() {
        // SAFETY: called in single-threaded test context
        unsafe { std::env::remove_var("PAY_MAINNET_RPC_URL") };
        assert_eq!(mainnet_rpc_url(), "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn pay_api_url_default() {
        // SAFETY: called in single-threaded test context
        unsafe { std::env::remove_var("PAY_API_URL") };
        assert_eq!(pay_api_url(), DEFAULT_PAY_API_URL);
    }

    #[test]
    fn infer_network_classifies_local_and_mainnet() {
        assert_eq!(infer_network("http://127.0.0.1:8899"), "sandbox");
        assert_eq!(infer_network("http://localhost:8899"), "sandbox");
        assert_eq!(infer_network("https://402.surfnet.dev:8899"), "sandbox");
        assert_eq!(
            infer_network("https://api.mainnet-beta.solana.com"),
            "mainnet"
        );
        assert_eq!(infer_network("https://my-helius.example.com"), "mainnet");
    }

    #[test]
    fn account_balances_default() {
        let b = AccountBalances::default();
        assert_eq!(b.sol_lamports, 0);
        assert!(b.tokens.is_empty());
        assert!(!b.tokens_unavailable);
    }

    #[test]
    fn received_funds_has_any_sol() {
        let r = ReceivedFunds {
            sol_lamports: 100,
            tokens: vec![],
        };
        assert!(r.has_any());
    }

    #[test]
    fn received_funds_has_any_tokens() {
        let r = ReceivedFunds {
            sol_lamports: 0,
            tokens: vec![ReceivedToken {
                mint: "abc".to_string(),
                ui_amount: 1.0,
                symbol: None,
            }],
        };
        assert!(r.has_any());
    }

    #[test]
    fn received_funds_has_any_empty() {
        let r = ReceivedFunds {
            sol_lamports: 0,
            tokens: vec![],
        };
        assert!(!r.has_any());
    }

    #[test]
    fn diff_received_sol_increase() {
        let baseline = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 1_000_000);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_sol_decrease_is_zero() {
        let baseline = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 0);
    }

    #[test]
    fn diff_received_token_increase() {
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                raw_amount: 10_000_000,
                ui_amount: 10.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                raw_amount: 25_500_000,
                ui_amount: 25.5,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.tokens.len(), 1);
        assert!((diff.tokens[0].ui_amount - 15.5).abs() < f64::EPSILON);
        assert_eq!(diff.tokens[0].symbol, Some("USDC"));
    }

    #[test]
    fn diff_received_new_token() {
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "NEW_MINT".to_string(),
                raw_amount: 100_000_000,
                ui_amount: 100.0,
                symbol: None,
            }],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.tokens.len(), 1);
        assert!((diff.tokens[0].ui_amount - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn diff_received_no_change() {
        let balances = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 50_000_000,
                ui_amount: 50.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let diff = balances.diff_received(&balances);
        assert_eq!(diff.sol_lamports, 0);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_skips_token_diff_when_baseline_unavailable() {
        // Baseline was captured while pay-api was offline → its empty
        // tokens list is missing data, not a true zero. Diffing against a
        // healthy `current` that shows funds must NOT report a "received".
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 5_000_000,
                ui_amount: 5.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_skips_token_diff_when_current_unavailable() {
        // Mid-poll pay-api blip: current.tokens is empty but
        // tokens_unavailable=true. Don't report negative deltas as anything.
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 5_000_000,
                ui_amount: 5.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let diff = current.diff_received(&baseline);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_still_diffs_sol_when_tokens_unavailable() {
        // SOL comes from RPC, not pay-api, so it remains trustworthy even
        // when tokens_unavailable is set.
        let baseline = AccountBalances {
            sol_lamports: 100,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 1_000,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 900);
        assert!(diff.tokens.is_empty());
    }
}
