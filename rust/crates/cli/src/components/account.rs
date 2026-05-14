//! Reusable account/balance rendering — used by `pay whoami` and
//! `pay account ls` so the two views stay in sync.

use owo_colors::OwoColorize;
use pay_core::client::balance::AccountBalances;

/// Render `<name> [<network> <pubkey>]` with the bracketed location dimmed.
///
/// `name` is taken pre-styled (the caller decides bold / colour for active
/// vs inactive vs highlighted rows) so this helper just concatenates.
pub fn format_account_header(name_rendered: &str, network: &str, pubkey: &str) -> String {
    format!(
        "{} {}",
        name_rendered,
        format!("[{network} {pubkey}]").dimmed()
    )
}

/// Print one stablecoin balance per line under `indent`. When all balances
/// are zero, prints nothing and returns `false` — callers use the return
/// value to decide whether to surface a trailing "run `pay topup`" note.
pub fn print_balances(balances: &AccountBalances, indent: &str) -> bool {
    if balances.tokens.is_empty() {
        return false;
    }
    for t in &balances.tokens {
        let symbol = t.symbol.unwrap_or("?");
        eprintln!(
            "{indent}- {:<6} {}",
            symbol,
            format!("{:.2}", t.ui_amount).green()
        );
    }
    true
}

/// Fallback rendered when balance lookup failed (RPC down, pay-api
/// unreachable, etc). Prints "api offline" in yellow followed by a clickable
/// block-explorer link routed by network family.
pub fn print_balance_unavailable(
    indent: &str,
    pubkey: Option<&str>,
    network: &str,
    rpc_url: &str,
) {
    eprintln!(
        "{indent}{}  {}",
        "api offline".yellow(),
        explorer_link(pubkey, network, rpc_url)
    );
}

/// Yellow trailing note shown when every mainnet balance the caller looked at
/// came back empty. Both `pay whoami` and `pay accounts` print this once at
/// the bottom of their output.
pub fn print_topup_note() {
    eprintln!();
    eprintln!("{}", "run `pay topup` to fund a mainnet account".yellow());
}

/// Clickable terminal hyperlink to the block explorer for `pubkey`,
/// routed by network family: EVM slugs → etherscan/basescan/etc.,
/// Solana slugs → explorer.solana.com with `cluster=custom` when the
/// caller is on a non-mainnet RPC. Returns `—` (dimmed) when no pubkey is
/// available.
pub fn explorer_link(pubkey: Option<&str>, network: &str, rpc_url: &str) -> String {
    match pubkey {
        Some(pk) if !pk.is_empty() => {
            let url = if pay_core::accounts::is_evm_network_family(network) {
                evm_explorer_url(network, pk)
            } else {
                solana_explorer_url(network, pk, rpc_url)
            };
            format!("\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\", "balance ↗".dimmed())
        }
        _ => "—".dimmed().to_string(),
    }
}

fn evm_explorer_url(network: &str, address: &str) -> String {
    let base = match network {
        "ethereum" => "https://etherscan.io/address",
        "base" => "https://basescan.org/address",
        "optimism" => "https://optimistic.etherscan.io/address",
        "arbitrum" => "https://arbiscan.io/address",
        "sepolia" => "https://sepolia.etherscan.io/address",
        "holesky" => "https://holesky.etherscan.io/address",
        "base-sepolia" => "https://sepolia.basescan.org/address",
        _ => "https://etherscan.io/address",
    };
    format!("{base}/{address}")
}

fn solana_explorer_url(network: &str, pubkey: &str, rpc_url: &str) -> String {
    let base = format!("https://explorer.solana.com/address/{pubkey}/tokens");
    // Slug wins over RPC URL substring: an empty or custom RPC on
    // `mainnet` must not get `cluster=custom`, and a non-mainnet slug must
    // never reuse the mainnet explorer view.
    if network == "mainnet" || rpc_url.contains("mainnet") {
        base
    } else {
        let encoded = percent_encode_rpc(rpc_url);
        format!("{base}?cluster=custom&customUrl={encoded}")
    }
}

fn percent_encode_rpc(url: &str) -> String {
    url.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_link_for_evm_networks_uses_correct_block_explorer() {
        let link_eth = explorer_link(Some("0xabc"), "ethereum", "");
        assert!(link_eth.contains("etherscan.io/address/0xabc"));

        let link_base = explorer_link(Some("0xabc"), "base", "");
        assert!(link_base.contains("basescan.org/address/0xabc"));

        let link_sepolia = explorer_link(Some("0xabc"), "sepolia", "");
        assert!(link_sepolia.contains("sepolia.etherscan.io/address/0xabc"));

        let link_base_sepolia = explorer_link(Some("0xabc"), "base-sepolia", "");
        assert!(link_base_sepolia.contains("sepolia.basescan.org/address/0xabc"));
    }

    #[test]
    fn explorer_link_for_solana_mainnet_omits_cluster_param() {
        let link = explorer_link(Some("ABC"), "mainnet", "https://api.mainnet-beta.solana.com");
        assert!(link.contains("explorer.solana.com/address/ABC/tokens"));
        assert!(!link.contains("cluster=custom"));
    }

    #[test]
    fn explorer_link_for_solana_devnet_appends_cluster_param() {
        let link = explorer_link(Some("ABC"), "devnet", "https://api.devnet.solana.com");
        assert!(link.contains("cluster=custom"));
        assert!(link.contains("customUrl=https%3A%2F%2Fapi.devnet.solana.com"));
    }

    #[test]
    fn explorer_link_for_empty_pubkey_renders_dash() {
        let link = explorer_link(None, "mainnet", "");
        assert!(link.contains('—'));
    }

    #[test]
    fn explorer_link_slug_wins_over_rpc_substring() {
        // EVM slug must never reuse the Solana explorer even if the RPC URL
        // happens to mention "mainnet".
        let link = explorer_link(Some("0xabc"), "base", "https://mainnet.base.org");
        assert!(link.contains("basescan.org"));
        assert!(!link.contains("explorer.solana.com"));
    }
}
