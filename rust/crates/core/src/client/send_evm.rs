//! EVM `pay send` — direct ERC-20 `transfer` from the user's wallet.
//!
//! Phase 12 closes the UX gap where `pay send --network sepolia ...` used to
//! hard-reject. The flow is intentionally minimal: load the configured EVM
//! signer, build an ERC-20 `transfer(address,uint256)` call, broadcast via
//! the configured RPC, and wait for one confirmation. Gas is paid in native
//! ETH from the same wallet — there is no fee-payer / fee-refund split as in
//! the Solana flow.
//!
//! Memo and `--fee-within` semantics are unsupported here:
//! - ERC-20 transfer has no on-chain memo field; callers should attach
//!   side-channel metadata.
//! - Gas is paid in ETH separately from the stablecoin amount, so subtracting
//!   the "fee" from `amount` is meaningless.

#![cfg(feature = "evm")]

use crate::Result;
use crate::client::balance::{evm_rpc_url, evm_stablecoin_address, evm_stablecoin_decimals};

/// Caller-facing parameters. Mirrors `StablecoinSendRequest` on the Solana
/// side closely enough for the CLI to reuse parsing helpers without a
/// custom struct per chain.
pub struct EvmSendRequest<'a> {
    pub amount: &'a str,
    pub recipient: &'a str,
    pub stablecoin_symbol: &'a str,
    pub network: &'a str,
    pub account_override: Option<&'a str>,
}

/// Outcome of a successful EVM transfer. Names mirror `SendResult` where the
/// concept maps; `signature` is the 0x-prefixed 32-byte tx hash.
pub struct EvmSendResult {
    pub signature: String,
    pub amount_raw: u128,
    pub decimals: u8,
    pub currency: String,
    pub asset: String,
    pub from: String,
    pub to: String,
    pub network: String,
    pub rpc_url: String,
}

alloy::sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

/// Synchronous entry point — the CLI command stays sync and we manage the
/// tokio runtime here. Returns once the broadcast transaction has been
/// included with `confirmations >= 1`.
pub fn send_erc20(req: EvmSendRequest<'_>) -> Result<EvmSendResult> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| crate::Error::Config(format!("Failed to build runtime for EVM send: {e}")))?;
    rt.block_on(send_erc20_async(req))
}

async fn send_erc20_async(req: EvmSendRequest<'_>) -> Result<EvmSendResult> {
    use crate::accounts::FileAccountsStore;
    use crate::chain::ChainFamily;
    use alloy::primitives::{Address, U256};
    use alloy::providers::ProviderBuilder;
    use std::str::FromStr;

    // 1) Resolve chain id from the network slug; reject Solana up front.
    // The actual chain-id binding lives inside `EvmChainSigner` after
    // `load_evm_signer_for_network`, so we only validate the family here.
    if !matches!(
        ChainFamily::from_network_slug(req.network),
        ChainFamily::Evm { .. }
    ) {
        return Err(crate::Error::Config(format!(
            "`{}` is not an EVM network",
            req.network
        )));
    }

    // 2) Load the user's secp256k1 signer via the same path Phase 11 uses,
    //    so keystore prompts and ephemeral fall-through stay consistent.
    let store = FileAccountsStore::default_path();
    let (signer, _ephemeral) =
        crate::signer::load_evm_signer_for_network(req.network, &store, req.account_override)?;
    let from_addr = signer.signer.address();

    // 3) Token registry — symbol + network determines contract + decimals.
    let token_hex =
        evm_stablecoin_address(req.network, req.stablecoin_symbol).ok_or_else(|| {
            crate::Error::Config(format!(
                "{} is not deployed on `{}` in pay's token registry",
                req.stablecoin_symbol, req.network
            ))
        })?;
    let token_addr = Address::from_str(token_hex)
        .map_err(|e| crate::Error::Config(format!("Bad token address `{token_hex}`: {e}")))?;
    let decimals = evm_stablecoin_decimals(req.stablecoin_symbol).ok_or_else(|| {
        crate::Error::Config(format!(
            "Unknown decimal places for stablecoin `{}` — add it to evm_stablecoin_decimals",
            req.stablecoin_symbol
        ))
    })?;

    // 4) Recipient parse.
    let to_addr = Address::from_str(req.recipient).map_err(|e| {
        crate::Error::Config(format!(
            "Recipient `{}` is not a valid EVM address: {e}",
            req.recipient
        ))
    })?;

    // 5) Amount: parse string in human units, scale to base units. We support
    //    "max" so the caller can drain the balance without a second RPC.
    let rpc_url = evm_rpc_url(req.network);
    let parsed_rpc: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| crate::Error::Config(format!("Invalid EVM RPC URL `{rpc_url}`: {e}")))?;
    let wallet = alloy::network::EthereumWallet::from(signer.signer.clone());
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(parsed_rpc);

    let erc20 = IERC20::new(token_addr, &provider);

    let amount_raw: U256 = if req.amount.eq_ignore_ascii_case("max") {
        let bal = erc20.balanceOf(from_addr).call().await.map_err(|e| {
            crate::Error::Config(format!(
                "Could not read {} balance for `max`: {e}",
                req.stablecoin_symbol
            ))
        })?;
        if bal.is_zero() {
            return Err(crate::Error::Config(format!(
                "No {} to send (balance is zero)",
                req.stablecoin_symbol
            )));
        }
        bal
    } else {
        parse_evm_amount(req.amount, decimals)?
    };

    // 6) Broadcast `transfer(to, amount)` and wait for inclusion.
    let pending = erc20
        .transfer(to_addr, amount_raw)
        .send()
        .await
        .map_err(|e| crate::Error::Config(format!("eth_sendRawTransaction failed: {e}")))?;
    let receipt = pending
        .with_required_confirmations(1)
        .get_receipt()
        .await
        .map_err(|e| crate::Error::Config(format!("Confirmation wait failed: {e}")))?;

    if !receipt.status() {
        return Err(crate::Error::Config(format!(
            "Transfer reverted on-chain: tx {:#x}",
            receipt.transaction_hash
        )));
    }

    Ok(EvmSendResult {
        signature: format!("{:#x}", receipt.transaction_hash),
        amount_raw: u256_saturate_u128(amount_raw),
        decimals,
        currency: req.stablecoin_symbol.to_string(),
        asset: token_hex.to_string(),
        from: format!("{from_addr:?}"),
        to: format!("{to_addr:?}"),
        network: req.network.to_string(),
        rpc_url,
    })
}

/// Convert a decimal user string into base units. Uses alloy's
/// `parse_units` which accepts arbitrary decimal precision; we reject
/// negative values since `transfer` doesn't have negative semantics.
fn parse_evm_amount(s: &str, decimals: u8) -> Result<alloy::primitives::U256> {
    use alloy::primitives::utils::parse_units;
    let parsed = parse_units(s, decimals)
        .map_err(|e| crate::Error::Config(format!("Invalid amount `{s}`: {e}")))?;
    if parsed.is_negative() {
        return Err(crate::Error::Config(format!(
            "Amount `{s}` is negative — `pay send` only supports non-negative transfers"
        )));
    }
    Ok(parsed.get_absolute())
}

fn u256_saturate_u128(v: alloy::primitives::U256) -> u128 {
    if v <= alloy::primitives::U256::from(u128::MAX) {
        v.to::<u128>()
    } else {
        u128::MAX
    }
}
