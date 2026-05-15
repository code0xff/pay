#[cfg(feature = "evm")]
use owo_colors::OwoColorize;

use crate::{components, network::SolanaNetwork};

/// Import funds from Venmo, PayPal, or a mobile wallet.
#[derive(clap::Args)]
pub struct TopupCommand {
    /// Account address to receive funds. Defaults to your mainnet account.
    #[arg(long)]
    pub account: Option<String>,

    /// Use the sandbox (localnet) account instead of mainnet.
    #[arg(long)]
    pub sandbox: bool,
}

impl TopupCommand {
    pub fn run(self, network_override: Option<&str>) -> pay_core::Result<()> {
        // Phase 12-4: EVM networks get a faucet/wallet hint plus a short
        // balance-polling loop. They never enter the Solana TUI flow.
        if let Some(slug) = network_override
            && pay_core::accounts::is_evm_network_family(slug)
        {
            #[cfg(feature = "evm")]
            {
                return run_evm_topup(slug, self.account.as_deref());
            }
            #[cfg(not(feature = "evm"))]
            {
                return Err(pay_core::Error::Config(format!(
                    "`pay topup --network {slug}` requires the `evm` Cargo feature."
                )));
            }
        }

        let config = pay_core::Config::load().unwrap_or_default();

        let (network, rpc_url) = if self.sandbox {
            let url = config
                .rpc_url
                .clone()
                .unwrap_or_else(|| pay_core::config::SANDBOX_RPC_URL.to_string());
            ("localnet", url)
        } else {
            let url = config
                .rpc_url
                .clone()
                .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
            (pay_core::accounts::MAINNET_NETWORK, url)
        };

        let (pubkey, account_name) = if let Some(addr) = &self.account {
            (addr.clone(), addr.clone())
        } else {
            let accounts = pay_core::accounts::AccountsFile::load()?;
            match accounts.account_for_network(network) {
                Some((name, account)) => (
                    account.pubkey.clone().ok_or_else(|| {
                        pay_core::Error::Config("Account has no pubkey".to_string())
                    })?,
                    name.to_string(),
                ),
                None => {
                    return Err(pay_core::Error::Config(format!(
                        "No {network} account found. Run `pay setup` first."
                    )));
                }
            }
        };

        match crate::tui::run_topup_flow(&pubkey, &rpc_url, &account_name)? {
            Some(completion) => print_topup_success(&completion, network, &rpc_url),
            None => print_topup_aborted(&account_name),
        }
        Ok(())
    }
}

pub(crate) fn print_topup_success(
    completion: &crate::tui::TopupCompletion,
    network: &str,
    rpc_url: &str,
) {
    components::print_notice(
        components::NoticeLevel::Success,
        "Account funded",
        &topup_success_body(completion, network, rpc_url),
    );
}

fn print_topup_aborted(account_name: &str) {
    components::print_notice(
        components::NoticeLevel::Warning,
        "Top-up aborted",
        &topup_aborted_body(account_name),
    );
}

fn topup_aborted_body(account_name: &str) -> String {
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        topup_retry_command(account_name)
    )
}

pub(crate) fn topup_retry_command(account_name: &str) -> String {
    if account_name == pay_core::accounts::DEFAULT_ACCOUNT_NAME {
        "pay topup".to_string()
    } else {
        format!("pay topup --account {account_name}")
    }
}

pub(crate) fn topup_success_body(
    completion: &crate::tui::TopupCompletion,
    network: &str,
    rpc_url: &str,
) -> String {
    let mut lines = Vec::new();
    if let Some(amount) = topup_received_amount(&completion.received) {
        lines.push(format!("Received {amount}"));
    }
    if let Some(hash) = &completion.tx_hash {
        let cluster = SolanaNetwork::from_slug(network).explorer_cluster(rpc_url);
        lines.push(format!(
            "{} {hash}",
            components::solana_transaction_link(hash, &cluster)
        ));
    }
    if lines.is_empty() {
        lines.push("Funds received".to_string());
    }
    lines.join("\n")
}

pub(crate) fn topup_received_amount(
    received: &pay_core::client::balance::ReceivedFunds,
) -> Option<String> {
    let amount = crate::commands::account::new::format_received(received);
    (!amount.is_empty()).then_some(amount)
}

/// Hint string mapped by EVM network slug — testnet faucets get explicit
/// URLs; mainnets fall back to a generic wallet-or-exchange note since pay
/// doesn't itself broker fiat-on-ramp for EVM yet.
#[cfg(feature = "evm")]
fn evm_funding_hint(network: &str) -> &'static str {
    match network {
        "sepolia" => "Sepolia faucet: https://www.alchemy.com/faucets/ethereum-sepolia",
        "base-sepolia" => "Base-Sepolia faucet: https://www.alchemy.com/faucets/base-sepolia",
        "holesky" => "Holesky faucet: https://www.alchemy.com/faucets/ethereum-holesky",
        _ => "Send funds from MetaMask, Coinbase Wallet, or a centralized exchange.",
    }
}

#[cfg(feature = "evm")]
fn run_evm_topup(network: &str, account_override: Option<&str>) -> pay_core::Result<()> {
    use std::time::{Duration, Instant};

    let accounts = pay_core::accounts::AccountsFile::load()?;
    let (account_name, address) = match account_override {
        Some(name) => {
            let acct = accounts
                .named_account_for_network(network, name)
                .ok_or_else(|| {
                    pay_core::Error::Config(format!(
                        "No account `{name}` configured on {network}"
                    ))
                })?;
            let pubkey = acct.pubkey.clone().ok_or_else(|| {
                pay_core::Error::Config(format!(
                    "Account `{name}` on {network} has no pubkey"
                ))
            })?;
            (name.to_string(), pubkey)
        }
        None => match accounts.account_for_network(network) {
            Some((name, account)) => {
                let pubkey = account.pubkey.clone().ok_or_else(|| {
                    pay_core::Error::Config("Account has no pubkey".to_string())
                })?;
                (name.to_string(), pubkey)
            }
            None => {
                return Err(pay_core::Error::Config(format!(
                    "No {network} account found. Run `pay account new --chain-family evm --network {network}` first."
                )));
            }
        },
    };

    eprintln!();
    eprintln!(
        "  {}  {}  on  {}",
        "topup".dimmed(),
        address.green(),
        network.green()
    );
    eprintln!("  {}", evm_funding_hint(network).dimmed());
    eprintln!();
    eprintln!("  {}", "Polling for incoming USDC for 60 seconds (^C to abort)...".dimmed());

    // Read the baseline once so any inbound delta during the wait is
    // attributable to this topup attempt.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("Failed to build polling runtime: {e}")))?;
    let baseline_usdc = rt
        .block_on(pay_core::balance::get_evm_balances(network, &address))
        .ok()
        .and_then(usdc_raw_amount)
        .unwrap_or(0);

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(3));
        let current = match rt.block_on(pay_core::balance::get_evm_balances(network, &address)) {
            Ok(b) => usdc_raw_amount(b).unwrap_or(0),
            Err(_) => continue,
        };
        if current > baseline_usdc {
            let delta = current - baseline_usdc;
            // USDC is 6-decimal on every EVM chain pay tracks; if more
            // tokens land later, route this through evm_stablecoin_decimals.
            let display = format!("{:.6}", delta as f64 / 1_000_000.0);
            components::print_notice(
                components::NoticeLevel::Success,
                "Account funded",
                &format!("Received {} USDC at {}.", display, address),
            );
            return Ok(());
        }
    }

    components::print_notice(
        components::NoticeLevel::Warning,
        "No funds detected within 60s",
        &format!(
            "No incoming USDC observed at {address}.\n\
             Once the on-chain transfer mines, run:\n\
             $ pay --network {network} --account {account_name} whoami"
        ),
    );
    Ok(())
}

#[cfg(feature = "evm")]
fn usdc_raw_amount(balances: pay_core::client::balance::AccountBalances) -> Option<u64> {
    balances
        .tokens
        .into_iter()
        .find(|t| t.symbol == Some("USDC"))
        .map(|t| t.raw_amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topup_aborted_body_uses_default_topup_command_for_default_account() {
        assert_eq!(
            topup_aborted_body("default"),
            "A top-up is required before making paid requests.\n$ pay topup"
        );
    }

    #[test]
    fn topup_aborted_body_uses_named_account_topup_command() {
        assert_eq!(
            topup_aborted_body("test-2"),
            "A top-up is required before making paid requests.\n$ pay topup --account test-2"
        );
    }

    #[cfg(feature = "evm")]
    #[test]
    fn evm_funding_hint_uses_known_faucet_for_sepolia() {
        let hint = evm_funding_hint("sepolia");
        assert!(hint.contains("Sepolia"));
        assert!(hint.contains("https://"));
    }

    #[cfg(feature = "evm")]
    #[test]
    fn evm_funding_hint_falls_back_to_wallet_note_for_mainnet() {
        // Mainnet EVMs don't have a faucet; the hint should redirect the
        // user to an external wallet flow instead of fabricating a URL.
        let hint = evm_funding_hint("ethereum");
        assert!(hint.contains("MetaMask") || hint.contains("Wallet"));
        assert!(!hint.contains("faucet"));
    }
}
