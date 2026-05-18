//! Live Sepolia / Base-Sepolia integration tests for `get_evm_balances`.
//!
//! Gated under `evm` + `network_tests` so the default CI build doesn't hit
//! publicnode RPC. Run explicitly:
//!
//! ```bash
//! cargo test -p pay-core --features evm,network_tests --test evm_balance_tests
//! ```
//!
//! Optional environment overrides:
//! - `PAY_SEPOLIA_RPC_URL` — alternate Sepolia endpoint
//! - `PAY_BASE_SEPOLIA_RPC_URL` — alternate Base-Sepolia endpoint
//! - `PAY_EVM_TEST_ADDRESS` — funded test wallet (defaults to burn address)

#![cfg(feature = "network_tests")]

use pay_core::client::balance::get_evm_balances;

const BURN_ADDRESS: &str = "0x000000000000000000000000000000000000dEaD";

fn test_address() -> String {
    std::env::var("PAY_EVM_TEST_ADDRESS").unwrap_or_else(|_| BURN_ADDRESS.to_string())
}

#[tokio::test]
async fn sepolia_burn_address_returns_zero_or_minimal_balance() {
    let balances = get_evm_balances("sepolia", BURN_ADDRESS)
        .await
        .expect("Sepolia RPC call should succeed");

    // EVM has no Solana lamports.
    assert_eq!(balances.sol_lamports, 0, "EVM result must not carry SOL");
    assert!(
        !balances.tokens_unavailable,
        "tokens_unavailable should be false on a successful RPC roundtrip"
    );

    // Burn address occasionally has dust ETH/USDC. If anything shows up it
    // must be positive and a known symbol — catches a regression where we'd
    // emit a token row with NaN/0 UI amount from a malformed `balanceOf`
    // response.
    for token in &balances.tokens {
        assert!(
            token.ui_amount > 0.0,
            "tokens vec entries must be positive: {token:?}"
        );
        assert!(
            matches!(
                token.symbol.as_deref(),
                Some("ETH") | Some("USDC") | Some("USDT")
            ),
            "unexpected symbol from Sepolia: {token:?}"
        );
    }
}

#[tokio::test]
async fn sepolia_invalid_address_returns_clear_error() {
    let err = get_evm_balances("sepolia", "not-a-hex-address")
        .await
        .expect_err("malformed address must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("Invalid EVM address"),
        "error must surface the address-validation failure: {msg}"
    );
}

#[tokio::test]
async fn sepolia_eth_balance_uses_18_decimals() {
    let addr = test_address();
    let balances = get_evm_balances("sepolia", &addr)
        .await
        .expect("Sepolia RPC call should succeed");

    if let Some(eth) = balances
        .tokens
        .iter()
        .find(|t| t.symbol.as_deref() == Some("ETH"))
    {
        assert!(eth.ui_amount > 0.0, "ETH row present but ui_amount=0");
        // Regression guard: if `format_units` ever forgot to scale by 1e18 we'd
        // see raw wei (≥1e9 for any non-trivial balance) leak into ui_amount.
        // Even mainnet whale wallets stay well under 1e6 ETH.
        assert!(
            eth.ui_amount < 1_000_000.0,
            "ETH ui_amount {} looks like raw wei (decimals collapsed)",
            eth.ui_amount
        );
    }
}

#[tokio::test]
async fn sepolia_usdc_balanceof_does_not_panic() {
    // Exercises the ERC-20 `balanceOf` path. We don't assert a specific
    // balance — Sepolia USDC faucet values drift — only that the call
    // completes without panic or timeout. A panic here typically means the
    // alloy `sol!` macro lost sync with the IERC20 ABI between alloy bumps.
    let _ = get_evm_balances("sepolia", BURN_ADDRESS)
        .await
        .expect("Sepolia USDC balanceOf call must not panic");
}

#[tokio::test]
async fn base_sepolia_burn_address_smoke() {
    let balances = get_evm_balances("base-sepolia", BURN_ADDRESS)
        .await
        .expect("Base-Sepolia RPC call should succeed");
    assert!(
        !balances.tokens_unavailable,
        "Base-Sepolia roundtrip must mark tokens as available"
    );
    assert_eq!(balances.sol_lamports, 0);
}
