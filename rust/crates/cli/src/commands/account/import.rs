//! `pay account import` — import an account from a JSON key file.

use dialoguer::{Confirm, theme::ColorfulTheme};
use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Import an account from a JSON key file into a secure keystore.
#[derive(clap::Args)]
pub struct ImportCommand {
    /// Account name (required).
    pub name: String,

    /// Path to the JSON key file (Solana, 64-byte array format). Required
    /// for `--chain-family solana` (the default); ignored for EVM imports
    /// which read the key from `--secret-key-hex`.
    #[arg(value_name = "FILE")]
    pub file: Option<String>,

    /// Storage backend: "keychain", "gnome-keyring", or "windows-hello".
    #[arg(long)]
    pub backend: Option<String>,

    /// Legacy vault name.
    #[arg(long, hide = true)]
    pub vault: Option<String>,

    /// Chain family: `solana` (default) or `evm`.
    #[arg(long, value_name = "FAMILY")]
    pub chain_family: Option<String>,

    /// EVM network slug for `--chain-family evm` (e.g. `sepolia`, `base`).
    #[arg(long, value_name = "SLUG")]
    pub network: Option<String>,

    /// 32-byte secp256k1 private key as hex, with or without `0x` prefix.
    /// Required for `--chain-family evm`. The derived EIP-55 address is
    /// shown for confirmation before the key is sealed into the keystore.
    #[arg(long, value_name = "HEX")]
    pub secret_key_hex: Option<String>,
}

impl ImportCommand {
    pub fn run(self) -> pay_core::Result<()> {
        if self.chain_family.as_deref() == Some("evm") {
            #[cfg(feature = "evm")]
            {
                return self.run_evm();
            }
            #[cfg(not(feature = "evm"))]
            {
                return Err(pay_core::Error::Config(
                    "EVM imports require the `evm` Cargo feature. Rebuild with \
                     `cargo build -p pay --features evm`."
                        .to_string(),
                ));
            }
        }

        let theme = ColorfulTheme::default();
        let file_path = self.file.as_deref().ok_or_else(|| {
            pay_core::Error::Config(
                "`pay account import` needs a JSON keypair file for Solana \
                 (or `--chain-family evm --secret-key-hex 0x...` for EVM)."
                    .to_string(),
            )
        })?;

        // 1. Read and validate keypair
        let expanded = shellexpand::tilde(file_path);
        let data = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| pay_core::Error::Config(format!("Failed to read {file_path}: {e}")))?;
        let keypair_bytes: Vec<u8> = serde_json::from_str(&data)
            .map_err(|e| pay_core::Error::Config(format!("Invalid keypair JSON: {e}")))?;

        if keypair_bytes.len() != 64 {
            return Err(pay_core::Error::Config(format!(
                "Expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }

        let pubkey_b58 = bs58::encode(&keypair_bytes[32..64]).into_string();

        // 2. Display balance
        eprintln!();
        eprintln!("  {} {pubkey_b58}", "Pubkey:".dimmed());
        display_balance(&pubkey_b58);
        eprintln!();

        // 3. Check if this pubkey is already registered
        let mut accounts = pay_core::accounts::AccountsFile::load()?;
        if let Some((network, existing_name)) = find_account_by_pubkey(&accounts, &pubkey_b58) {
            let proceed = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "This key is already registered as \"{}\" on {}. Import anyway?",
                    existing_name.yellow(),
                    network.yellow(),
                ))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            if !proceed {
                eprintln!("Import cancelled.");
                return Ok(());
            }
        }

        // 4. Resolve account name — confirm overwrite if it already exists.
        let name = resolve_name(&theme, &self.name, &accounts)?;

        // 4. Pick backend and import
        let backend_id = match &self.backend {
            Some(b) => b.clone(),
            None => super::new::pick_backend()?,
        };

        let (ks, keystore_kind, _) =
            super::import::build_keystore(&backend_id, self.vault.as_deref())?;

        let sync = if backend_id == "1password" {
            pay_core::keystore::SyncMode::CloudSync
        } else {
            pay_core::keystore::SyncMode::ThisDeviceOnly
        };

        let intent = pay_core::keystore::AuthIntent::import_account(&name);
        ks.import_with_intent(&name, &keypair_bytes, sync, &intent)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        // 5. Save to accounts.yml under mainnet
        let is_first = accounts
            .accounts
            .get(pay_core::accounts::MAINNET_NETWORK)
            .is_none_or(|net| net.is_empty());

        accounts.upsert(
            pay_core::accounts::MAINNET_NETWORK,
            &name,
            pay_core::accounts::Account {
                keystore: keystore_kind,
                active: false,
                auth_required: Some(true),
                pubkey: Some(pubkey_b58),
                vault: self.vault,
                path: None,
                account: None,
                secret_key_b58: None,
                chain_family: None,
                secret_key_hex: None,
                created_at: None,
            },
        );

        // 6. Prompt for active (= mainnet default) if not the only account.
        let current_mainnet = accounts.default_account().map(|(n, _)| n.to_string());
        if !is_first && current_mainnet.as_deref() != Some(name.as_str()) {
            let make_default = Confirm::with_theme(&theme)
                .with_prompt(format!("Set '{}' as the default account?", name.green()))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            if make_default {
                accounts.set_active(pay_core::accounts::MAINNET_NETWORK, &name);
            }
        }

        accounts.save()?;

        // 7. Show the account list with the new entry highlighted
        super::list::print_account_list(
            &accounts,
            Some(super::list::Highlight::Green {
                network: pay_core::accounts::MAINNET_NETWORK,
                name: &name,
            }),
        );

        Ok(())
    }
}

impl ImportCommand {
    #[cfg(feature = "evm")]
    fn run_evm(self) -> pay_core::Result<()> {
        use pay_core::chain::{ChainFamily, ChainSigner, EvmChainSigner};

        let network = self.network.as_deref().ok_or_else(|| {
            pay_core::Error::Config(
                "`--chain-family evm` requires `--network <slug>` \
                 (e.g. `sepolia`, `base`, `base-sepolia`)."
                    .to_string(),
            )
        })?;
        if !pay_core::accounts::is_evm_network_family(network) {
            return Err(pay_core::Error::Config(format!(
                "`{network}` is not a recognized EVM network slug. \
                 Supported: ethereum, base, optimism, arbitrum, sepolia, \
                 holesky, base-sepolia."
            )));
        }
        let raw_hex = self.secret_key_hex.as_deref().ok_or_else(|| {
            pay_core::Error::Config(
                "`--chain-family evm` requires `--secret-key-hex <HEX>` \
                 (32 raw bytes, with or without 0x prefix)."
                    .to_string(),
            )
        })?;

        let chain_id = match ChainFamily::from_network_slug(network) {
            ChainFamily::Evm { chain_id } => chain_id,
            _ => unreachable!("is_evm_network_family already validated network slug"),
        };

        // EvmChainSigner::from_hex tolerates the leading 0x and validates
        // both length and curve order. Reuse it as the canonical parser so
        // CLI input drift stays in lockstep with the runtime signer.
        let signer = EvmChainSigner::from_hex(raw_hex, chain_id)
            .map_err(|e| pay_core::Error::Config(format!("Invalid secp256k1 private key: {e}")))?;
        let address = signer.address();
        let priv_bytes = signer.to_private_key_bytes();

        // Show the derived address before sealing the key so the user can
        // bail out if it doesn't match their expectation.
        eprintln!();
        eprintln!("  {} {}", "Address:".dimmed(), address);
        eprintln!(
            "  {} {} (chain id {chain_id})",
            "Network:".dimmed(),
            network.green()
        );
        eprintln!();

        let theme = ColorfulTheme::default();
        let mut accounts = pay_core::accounts::AccountsFile::load()?;
        if let Some((existing_network, existing_name)) = find_account_by_pubkey(&accounts, &address)
        {
            let proceed = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "This EVM address is already registered as \"{}\" on {}. Import anyway?",
                    existing_name.yellow(),
                    existing_network.yellow(),
                ))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;
            if !proceed {
                eprintln!("Import cancelled.");
                return Ok(());
            }
        }

        let backend_id = match &self.backend {
            Some(b) => b.clone(),
            None => super::new::pick_backend()?,
        };
        let (ks, keystore_kind, _) = build_keystore(&backend_id, self.vault.as_deref())?;

        if ks.evm_key_exists(&self.name) {
            let overwrite = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "EVM key for '{}' already exists in this keystore. Overwrite?",
                    self.name.yellow()
                ))
                .default(false)
                .interact()
                .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;
            if !overwrite {
                return Err(pay_core::Error::Config("Import cancelled.".to_string()));
            }
        }

        let intent = pay_core::keystore::AuthIntent::import_account(&self.name);
        ks.import_evm_key_with_intent(&self.name, &priv_bytes, &intent)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

        accounts.upsert(
            network,
            &self.name,
            pay_core::accounts::Account {
                keystore: keystore_kind,
                active: false,
                auth_required: Some(true),
                pubkey: Some(address.clone()),
                vault: self.vault.clone(),
                path: None,
                account: None,
                secret_key_b58: None,
                chain_family: Some("evm".to_string()),
                secret_key_hex: None,
                created_at: Some(chrono::Utc::now().to_rfc3339()),
            },
        );
        accounts.save()?;

        // Phase 12-2: match the Solana flow which prints `Balance: $X.XX USDC`
        // before the account list. EVM imports were silent on balance, which
        // made the user wonder whether the import had actually registered.
        display_evm_balance(network, &address);

        super::list::print_account_list(
            &accounts,
            Some(super::list::Highlight::Green {
                network,
                name: &self.name,
            }),
        );
        Ok(())
    }
}

#[cfg(feature = "evm")]
fn display_evm_balance(network: &str, address: &str) {
    // The EVM balance fetch is async and the surrounding command is
    // synchronous, so spin up a small runtime. The user already paid a
    // similar latency on `pay account list` so this won't surprise anyone.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("  {} balance lookup runtime: {e}", "!".yellow());
            return;
        }
    };
    let bal = rt.block_on(pay_core::balance::get_evm_balances(network, address));
    let rpc_url = pay_core::balance::evm_rpc_url(network);
    let display = match bal {
        Ok(b) => super::list::format_balance_display(Some(&b), Some(address), network, &rpc_url),
        Err(e) => format!("(unavailable: {e})"),
    };
    eprintln!("  {}  {}", "Balance:".dimmed(), display);
}

fn find_account_by_pubkey<'a>(
    accounts: &'a pay_core::accounts::AccountsFile,
    pubkey: &str,
) -> Option<(&'a str, &'a str)> {
    for (network, net_accounts) in &accounts.accounts {
        for (name, account) in net_accounts {
            if account.pubkey.as_deref() == Some(pubkey) {
                return Some((network, name));
            }
        }
    }
    None
}

fn display_balance(pubkey: &str) {
    let config = pay_core::Config::load().unwrap_or_default();
    let rpc_url = config
        .rpc_url
        .clone()
        .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
    let bal = super::list::fetch_balance(pubkey);
    let display = super::list::format_balance_display(
        bal.as_ref(),
        Some(pubkey),
        pay_core::accounts::MAINNET_NETWORK,
        &rpc_url,
    );
    eprintln!("  {}  {}", "Balance:".dimmed(), display);
}

fn resolve_name(
    theme: &ColorfulTheme,
    name: &str,
    accounts: &pay_core::accounts::AccountsFile,
) -> pay_core::Result<String> {
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let exists = accounts
        .accounts
        .get(pay_core::accounts::MAINNET_NETWORK)
        .is_some_and(|net| net.contains_key(name));

    if exists && has_tty {
        let overwrite = Confirm::with_theme(theme)
            .with_prompt(format!(
                "Account '{}' already exists. Overwrite?",
                name.yellow()
            ))
            .default(false)
            .interact()
            .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

        if !overwrite {
            return Err(pay_core::Error::Config("Import cancelled.".to_string()));
        }
    }
    Ok(name.to_string())
}

pub(super) fn build_keystore(
    backend_id: &str,
    vault: Option<&str>,
) -> pay_core::Result<(Keystore, pay_core::accounts::Keystore, &'static str)> {
    match backend_id {
        #[cfg(target_os = "macos")]
        "keychain" => Ok((
            Keystore::apple_keychain(),
            pay_core::accounts::Keystore::AppleKeychain,
            "Stored in macOS Keychain.",
        )),
        #[cfg(not(target_os = "macos"))]
        "keychain" => Err(pay_core::Error::Config(
            "Keychain is only available on macOS".into(),
        )),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => {
            crate::commands::setup::install_linux_polkit_policy_if_needed()?;
            Ok((
                Keystore::gnome_keyring(),
                pay_core::accounts::Keystore::GnomeKeyring,
                "Stored in GNOME Keyring.",
            ))
        }
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config(
            "GNOME Keyring is only available on Linux".into(),
        )),

        #[cfg(target_os = "windows")]
        "windows-hello" => Ok((
            Keystore::windows_hello(),
            pay_core::accounts::Keystore::WindowsHello,
            "Stored in Windows Credential Manager.",
        )),
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config(
            "Windows Hello is only available on Windows".into(),
        )),

        "1password" => {
            let op_account = super::new::resolve_op_account()?;
            let ks = match vault {
                Some(v) => Keystore::onepassword_with_vault(v, op_account),
                None => Keystore::onepassword(op_account),
            };
            Ok((
                ks,
                pay_core::accounts::Keystore::OnePassword,
                "Stored in 1Password.",
            ))
        }

        other => Err(pay_core::Error::Config(format!("Unknown backend: {other}"))),
    }
}
