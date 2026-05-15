//! `pay account new` — generate a fresh keypair and store it.

use dialoguer::Select;
use owo_colors::OwoColorize;
use pay_core::keystore::Keystore;

/// Generate a new keypair and store it securely.
#[derive(clap::Args)]
pub struct NewCommand {
    /// Account name (required).
    pub name: String,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// or "windows-hello" (Windows).
    #[arg(long)]
    pub backend: Option<String>,

    /// Legacy vault name.
    #[arg(long, hide = true)]
    pub vault: Option<String>,

    /// Replace existing account.
    #[arg(long)]
    pub force: bool,

    /// Chain family: `solana` (default) or `evm`. EVM generates a secp256k1
    /// key and stores it under the OS keystore's `evm-key:` entries. Requires
    /// `--network` to be set to an EVM slug (e.g. `sepolia`, `base`).
    #[arg(long, value_name = "FAMILY")]
    pub chain_family: Option<String>,

    /// EVM network slug for `--chain-family evm`. Ignored for Solana, which
    /// always provisions on mainnet from `pay account new`.
    #[arg(long, value_name = "SLUG")]
    pub network: Option<String>,
}

impl NewCommand {
    pub fn run(self) -> pay_core::Result<()> {
        if self.chain_family.as_deref() == Some("evm") {
            #[cfg(feature = "evm")]
            {
                return self.run_evm();
            }
            #[cfg(not(feature = "evm"))]
            {
                return Err(pay_core::Error::Config(
                    "EVM accounts require the `evm` Cargo feature. Rebuild with \
                     `cargo build -p pay --features evm`."
                        .to_string(),
                ));
            }
        }

        let (pubkey, backend_name) = create_account(
            &self.name,
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;
        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        let completion = crate::tui::run_topup_flow(&pubkey, &rpc_url, &self.name)?;
        print_next_steps(
            &self.name,
            backend_name,
            completion.as_ref().map(|c| &c.received),
        );
        Ok(())
    }

    #[cfg(feature = "evm")]
    fn run_evm(&self) -> pay_core::Result<()> {
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

        let (address, backend_display) = create_evm_account(
            &self.name,
            network,
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;
        print_evm_next_steps(&self.name, network, &address, backend_display);
        Ok(())
    }
}

/// Core account creation logic. Returns the base58 pubkey on success.
/// Shared by `pay account new` and `pay setup`.
/// Returns `(pubkey_b58, backend_display_name)`.
pub fn create_account(
    name: &str,
    backend: Option<&str>,
    vault: Option<&str>,
    force: bool,
) -> pay_core::Result<(String, &'static str)> {
    let backend_id = match backend {
        Some(b) => b.to_string(),
        None => pick_backend()?,
    };

    let (ks, keystore_kind, backend_display, op_info) = build_keystore(&backend_id, vault)?;

    if ks.exists(name) && !force {
        let pubkey = ks
            .pubkey(name)
            .map_err(|e| pay_core::Error::Config(format!("{e}")))?;
        let pubkey_b58 = bs58::encode(&pubkey).into_string();
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Info,
            "Account already exists",
            &format!(
                "`{name}` is already stored in {backend_display}.\nUse --force to replace it."
            ),
        );

        // Ensure the account is registered in accounts.yml even if the
        // keypair already exists in the keystore (e.g. after a reset).
        save_account(
            name,
            keystore_kind,
            &pubkey_b58,
            op_info.as_ref().and_then(|i| i.vault.clone()),
            None,
            op_info.as_ref().and_then(|i| i.account.clone()),
        )?;

        return Ok((pubkey_b58, backend_display));
    }

    let (keypair_bytes, pubkey_b58) = generate_keypair();

    let sync = if backend_id == "1password" {
        pay_core::keystore::SyncMode::CloudSync
    } else {
        pay_core::keystore::SyncMode::ThisDeviceOnly
    };

    let intent = pay_core::keystore::AuthIntent::create_account(name);
    ks.import_with_intent(name, &keypair_bytes, sync, &intent)
        .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

    save_account(
        name,
        keystore_kind,
        &pubkey_b58,
        op_info
            .as_ref()
            .and_then(|i| i.vault.clone())
            .or(vault.map(|v| v.to_string())),
        None,
        op_info.as_ref().and_then(|i| i.account.clone()),
    )?;

    Ok((pubkey_b58, backend_display))
}

/// Resolved 1Password account info for storing in accounts.yml.
pub struct OpAccountInfo {
    pub vault: Option<String>,
    pub account: Option<String>,
}

fn build_keystore(
    backend_id: &str,
    vault: Option<&str>,
) -> pay_core::Result<(
    Keystore,
    pay_core::accounts::Keystore,
    &'static str,
    Option<OpAccountInfo>,
)> {
    match backend_id {
        #[cfg(target_os = "macos")]
        "keychain" => Ok((
            Keystore::apple_keychain(),
            pay_core::accounts::Keystore::AppleKeychain,
            "Apple Keychain",
            None,
        )),
        #[cfg(not(target_os = "macos"))]
        "keychain" => Err(pay_core::Error::Config(
            "Keychain is only available on macOS".to_string(),
        )),

        #[cfg(target_os = "linux")]
        "gnome-keyring" => {
            if !Keystore::gnome_keyring_available() {
                return Err(pay_core::Error::Config(
                    "GNOME Keyring is not available.".to_string(),
                ));
            }
            crate::commands::setup::install_linux_polkit_policy_if_needed()?;
            Ok((
                Keystore::gnome_keyring(),
                pay_core::accounts::Keystore::GnomeKeyring,
                "GNOME Keyring",
                None,
            ))
        }
        #[cfg(not(target_os = "linux"))]
        "gnome-keyring" => Err(pay_core::Error::Config(
            "GNOME Keyring is only available on Linux".to_string(),
        )),

        #[cfg(target_os = "windows")]
        "windows-hello" => {
            if !Keystore::windows_hello_available() {
                return Err(pay_core::Error::Config(
                    "Windows Hello is not configured.".to_string(),
                ));
            }
            Ok((
                Keystore::windows_hello(),
                pay_core::accounts::Keystore::WindowsHello,
                "Windows Hello",
                None,
            ))
        }
        #[cfg(not(target_os = "windows"))]
        "windows-hello" => Err(pay_core::Error::Config(
            "Windows Hello is only available on Windows".to_string(),
        )),

        "1password" => {
            if !Keystore::onepassword_available() {
                return Err(pay_core::Error::Config(
                    "1Password CLI (`op`) is not installed or not signed in.".to_string(),
                ));
            }
            let op_account = resolve_op_account()?;
            let ks = match vault {
                Some(v) => Keystore::onepassword_with_vault(v, op_account.clone()),
                None => Keystore::onepassword(op_account.clone()),
            };
            Ok((
                ks,
                pay_core::accounts::Keystore::OnePassword,
                "1Password",
                Some(OpAccountInfo {
                    vault: vault.map(|v| v.to_string()),
                    account: op_account,
                }),
            ))
        }

        other => Err(pay_core::Error::Config(format!(
            "Unknown backend: {other}. Use {}.",
            available_backends_hint()
        ))),
    }
}

/// Comma-separated list of backends that work on the current OS.
/// Used in error messages so we don't suggest `keychain` to a Linux user.
fn available_backends_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "'keychain'"
    } else if cfg!(target_os = "linux") {
        "'gnome-keyring'"
    } else if cfg!(target_os = "windows") {
        "'windows-hello'"
    } else {
        "a supported platform backend"
    }
}

/// Resolve which 1Password account to use. If only one account is
/// configured, use it automatically. If multiple, prompt the user.
pub fn resolve_op_account() -> pay_core::Result<Option<String>> {
    let output = std::process::Command::new("op")
        .args(["account", "list", "--format=json"])
        .output()
        .map_err(|e| pay_core::Error::Config(format!("op account list: {e}")))?;

    if !output.status.success() {
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct OpAccount {
        account_uuid: String,
        email: String,
        url: String,
    }

    let accounts: Vec<OpAccount> = serde_json::from_slice(&output.stdout).unwrap_or_default();

    match accounts.len() {
        0 => Ok(None),
        1 => Ok(Some(accounts[0].account_uuid.clone())),
        _ => {
            let labels: Vec<String> = accounts
                .iter()
                .map(|a| format!("{} ({})", a.email, a.url))
                .collect();

            let selection =
                dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("Which 1Password account?")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;

            Ok(Some(accounts[selection].account_uuid.clone()))
        }
    }
}

/// Interactive backend picker. Returns the backend id string.
pub fn pick_backend() -> pay_core::Result<String> {
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !has_tty {
        return Err(pay_core::Error::Config(format!(
            "No --backend specified and no interactive terminal available.\n  \
             Pass --backend=<one of {}>.",
            available_backends_hint()
        )));
    }

    struct Opt {
        id: &'static str,
        label: String,
    }

    // Only show platform-native backend on the current OS
    #[cfg(target_os = "macos")]
    let options = [Opt {
        id: "keychain",
        label: "macOS Keychain (requires Touch ID)".into(),
    }];

    #[cfg(target_os = "linux")]
    let options = {
        if Keystore::gnome_keyring_available() {
            vec![Opt {
                id: "gnome-keyring",
                label: "GNOME Keyring (password prompt)".into(),
            }]
        } else {
            Vec::new()
        }
    };

    #[cfg(target_os = "windows")]
    let options = {
        if Keystore::windows_hello_available() {
            vec![Opt {
                id: "windows-hello",
                label: "Windows Hello (fingerprint / face / PIN)".into(),
            }]
        } else {
            Vec::new()
        }
    };

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let options: Vec<Opt> = Vec::new();

    if options.is_empty() {
        return Err(pay_core::Error::Config(
            "No supported keystore backend is available on this system.".to_string(),
        ));
    }

    let items: Vec<String> = options.iter().map(|o| o.label.clone()).collect();

    eprintln!();
    let selection = Select::new()
        .with_prompt("Where should pay store your account?")
        .items(&items)
        .default(0)
        .interact()
        .map_err(|e| pay_core::Error::Config(format!("Selection cancelled: {e}")))?;

    Ok(options[selection].id.to_string())
}

pub fn save_account(
    name: &str,
    keystore: pay_core::accounts::Keystore,
    pubkey: &str,
    vault: Option<String>,
    path: Option<String>,
    account: Option<String>,
) -> pay_core::Result<()> {
    let mut accounts = pay_core::accounts::AccountsFile::load()?;
    accounts.upsert(
        pay_core::accounts::MAINNET_NETWORK,
        name,
        pay_core::accounts::Account {
            keystore,
            active: false,
            auth_required: Some(true),
            pubkey: Some(pubkey.to_string()),
            vault,
            account,
            path,
            secret_key_b58: None,
            chain_family: None,
            secret_key_hex: None,
            created_at: None,
        },
    );
    accounts.save()
}

/// Print the post-setup summary and next-step hints.
///
/// Shows `✔` confirmation lines for keystore and (if funded) the received
/// amount. Skips the topup hint when the user already funded during setup.
pub fn print_next_steps(
    name: &str,
    backend_name: &str,
    received: Option<&pay_core::client::balance::ReceivedFunds>,
) {
    eprintln!();
    eprintln!(
        "  {} Account secured in {}",
        "✔".green(),
        backend_name.green()
    );

    if let Some(r) = received {
        let amount = format_received(r);
        if !amount.is_empty() {
            eprintln!("  {} Account funded with {}", "✔".green(), amount.green());
        }
        eprintln!();
        eprintln!(
            "  {}",
            "Ready to go. Time to make HTTP pay for itself.".dimmed()
        );
        eprintln!();
        eprintln!("  {}", "$ pay claude".bold());
        eprintln!("  {}", "$ pay codex".bold());
    } else {
        eprintln!();
        crate::components::print_notice(
            crate::components::NoticeLevel::Warning,
            "Top-up required",
            &topup_required_body(name),
        );
    }

    eprintln!();
}

fn topup_required_body(name: &str) -> String {
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        crate::commands::topup::topup_retry_command(name)
    )
}

pub fn format_received(r: &pay_core::client::balance::ReceivedFunds) -> String {
    if let Some(usdc) = r.tokens.iter().find(|t| t.symbol == Some("USDC")) {
        return format!("${:.2}", usdc.ui_amount);
    }
    if let Some(token) = r.tokens.first() {
        let sym = token.symbol.unwrap_or("tokens");
        return format!("{:.2} {sym}", token.ui_amount);
    }
    if r.sol_lamports > 0 {
        return format!("{:.4} SOL", r.sol_lamports as f64 / 1_000_000_000.0);
    }
    String::new()
}

pub fn generate_keypair() -> (Vec<u8>, String) {
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let mut keypair_bytes = Vec::with_capacity(64);
    keypair_bytes.extend_from_slice(&signing_key.to_bytes());
    keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

    let pubkey_b58 = bs58::encode(&verifying_key.to_bytes()).into_string();
    (keypair_bytes, pubkey_b58)
}

/// Generate a new secp256k1 keypair for `network`, store the 32-byte private
/// key under the OS keystore's `evm-key:` entries, and persist the
/// `chain_family: evm` entry in accounts.yml. Returns `(eip-55 address,
/// backend_display_name)`.
#[cfg(feature = "evm")]
pub fn create_evm_account(
    name: &str,
    network: &str,
    backend: Option<&str>,
    vault: Option<&str>,
    force: bool,
) -> pay_core::Result<(String, &'static str)> {
    use pay_core::chain::{ChainFamily, ChainSigner, EvmChainSigner};

    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => {
            return Err(pay_core::Error::Config(format!(
                "`{network}` is not an EVM network"
            )));
        }
    };

    let backend_id = match backend {
        Some(b) => b.to_string(),
        None => pick_backend()?,
    };
    let (ks, keystore_kind, backend_display, op_info) = build_keystore(&backend_id, vault)?;

    if ks.evm_key_exists(name) && !force {
        return Err(pay_core::Error::Config(format!(
            "EVM key for `{name}` already exists in {backend_display}. \
             Pass --force to replace it."
        )));
    }

    let signer = EvmChainSigner::random(chain_id);
    let priv_bytes = signer.to_private_key_bytes();
    let address = signer.address();

    let intent = pay_core::keystore::AuthIntent::create_account(name);
    ks.import_evm_key_with_intent(name, &priv_bytes, &intent)
        .map_err(|e| pay_core::Error::Config(format!("{e}")))?;

    save_evm_account(
        name,
        network,
        keystore_kind,
        &address,
        op_info
            .as_ref()
            .and_then(|i| i.vault.clone())
            .or(vault.map(|v| v.to_string())),
        op_info.as_ref().and_then(|i| i.account.clone()),
    )?;

    Ok((address, backend_display))
}

#[cfg(feature = "evm")]
fn save_evm_account(
    name: &str,
    network: &str,
    keystore: pay_core::accounts::Keystore,
    address: &str,
    vault: Option<String>,
    account: Option<String>,
) -> pay_core::Result<()> {
    let mut accounts = pay_core::accounts::AccountsFile::load()?;
    accounts.upsert(
        network,
        name,
        pay_core::accounts::Account {
            keystore,
            active: false,
            auth_required: Some(true),
            pubkey: Some(address.to_string()),
            vault,
            account,
            path: None,
            secret_key_b58: None,
            chain_family: Some("evm".to_string()),
            secret_key_hex: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
        },
    );
    accounts.save()
}

#[cfg(feature = "evm")]
fn print_evm_next_steps(
    name: &str,
    network: &str,
    address: &str,
    backend_name: &str,
) {
    eprintln!();
    eprintln!(
        "  {} EVM account `{}` secured in {}",
        "✔".green(),
        name.green(),
        backend_name.green()
    );
    eprintln!("  {} {} on {}", "address".dimmed(), address, network.green());
    eprintln!();
    crate::components::print_notice(
        crate::components::NoticeLevel::Info,
        "Fund the wallet",
        &format!(
            "EVM accounts can't be funded with `pay topup` yet — use a wallet \
             like MetaMask or a {network} faucet, then re-run with:\n\
             $ pay --network {network} --account {name} whoami",
        ),
    );
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topup_required_body_uses_default_topup_command_for_default_account() {
        assert_eq!(
            topup_required_body("default"),
            "A top-up is required before making paid requests.\n$ pay topup"
        );
    }

    #[test]
    fn topup_required_body_uses_named_account_topup_command() {
        assert_eq!(
            topup_required_body("test-2"),
            "A top-up is required before making paid requests.\n$ pay topup --account test-2"
        );
    }
}
