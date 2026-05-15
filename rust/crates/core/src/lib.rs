// Shared modules
pub mod accounts;
pub mod config;
pub mod error;
pub mod instructions;
pub mod keystore;
pub mod signer;
pub mod skills;

// EVM chain abstraction — gated behind the `evm` Cargo feature so the
// Solana-only build pulls zero alloy / x402-chain-eip155 transitive crates.
#[cfg(feature = "evm")]
pub mod chain;

// Client modules (CLI)
pub mod client;

// Flat re-exports so callers can use `pay_core::mpp`, `pay_core::runner`, etc.
pub use client::balance;
pub use client::fetch;
pub use client::mpp;
pub use client::runner;
pub use client::runner::{
    run_curl, run_curl_with_headers, run_httpie, run_httpie_with_headers, run_wget,
    run_wget_with_headers,
};
pub use client::sandbox;
pub use client::send;
pub use client::session;
pub use client::x402;

// Server modules (gateway proxy)
pub mod server;

pub use config::{Config, LogFormat};
pub use error::{Error, Result};
pub use server::{AccountingKey, AccountingStore, InMemoryStore, current_period};

#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(feature = "server")]
pub use solana_mpp;
#[cfg(feature = "server")]
pub use solana_x402;
#[cfg(feature = "server")]
use solana_mpp::server::Mpp;
#[cfg(feature = "server")]
use solana_x402::server::X402;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    fn mpp(&self) -> Option<&Mpp>;
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpp().into_iter().collect()
    }
    /// x402 server instances for this gateway, one per accepted currency.
    /// Returned in the same order as `mpps()` for consistency. The default
    /// implementation returns an empty list so existing MPP-only callers
    /// don't need to be updated.
    fn x402s(&self) -> Vec<&X402> {
        Vec::new()
    }
    /// External x402 facilitator the gateway delegates EVM verify+settle to.
    /// `None` for Solana-only gateways (Solana x402 settles via the in-process
    /// SDK and never touches a facilitator).
    fn facilitator(&self) -> Option<&server::x402_facilitator::FacilitatorClient> {
        None
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        None
    }
    fn session_mpp(&self) -> Option<&server::session::SessionMpp> {
        None
    }
    fn fee_payer_wallet(&self) -> Option<&server::telemetry::FeePayerWallet> {
        None
    }
    /// EVM in-flight `(chain_id, from, nonce)` lock used to close the race
    /// between `facilitator.settle` kicking off and the on-chain authorization
    /// state flipping to `true`. `None` for Solana-only gateways. The EVM
    /// x402 middleware requires `Some(...)` and will fail closed when the
    /// gateway forgot to wire it.
    #[cfg(feature = "evm")]
    fn evm_in_flight(&self) -> Option<&server::in_flight::InFlight> {
        None
    }
}
