// Shared modules
pub mod accounts;
pub mod config;
pub mod error;
pub mod instructions;
pub mod keystore;
pub mod signer;
pub mod skills;

// Chain family abstraction (EVM unconditional, Solana variant gated).
pub mod chain;

// Client modules (CLI)
pub mod client;

// Flat re-exports so callers can use `pay_core::runner`, etc.
pub use client::balance;
pub use client::fetch;
#[cfg(feature = "solana")]
pub use client::mpp;
pub use client::runner;
pub use client::runner::{
    run_curl, run_curl_with_headers, run_httpie, run_httpie_with_headers, run_wget,
    run_wget_with_headers,
};
#[cfg(feature = "solana")]
pub use client::sandbox;
#[cfg(feature = "solana")]
pub use client::send;
#[cfg(feature = "solana")]
pub use client::session;
pub use client::x402;

// Server modules (gateway proxy)
pub mod server;

pub use config::{Config, LogFormat};
pub use error::{Error, Result};
pub use server::{AccountingKey, AccountingStore, InMemoryStore, current_period};

#[cfg(feature = "server")]
use pay_types::metering::ApiSpec;
#[cfg(all(feature = "server", feature = "solana"))]
pub use solana_mpp;
#[cfg(all(feature = "server", feature = "solana"))]
use solana_mpp::server::Mpp;
#[cfg(all(feature = "server", feature = "solana"))]
pub use solana_x402;
#[cfg(all(feature = "server", feature = "solana"))]
use solana_x402::server::X402;

/// Trait that the application state must implement for the payment middleware.
#[cfg(feature = "server")]
pub trait PaymentState: Clone + Send + Sync + 'static {
    fn apis(&self) -> &[ApiSpec];
    #[cfg(feature = "solana")]
    fn mpp(&self) -> Option<&Mpp>;
    #[cfg(feature = "solana")]
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpp().into_iter().collect()
    }
    /// x402 server instances for this gateway, one per accepted currency.
    /// Returned in the same order as `mpps()` for consistency.
    #[cfg(feature = "solana")]
    fn x402s(&self) -> Vec<&X402> {
        Vec::new()
    }
    /// External x402 facilitator the gateway delegates EVM verify+settle to.
    /// `None` when no EVM facilitator is configured.
    fn facilitator(&self) -> Option<&server::x402_facilitator::FacilitatorClient> {
        None
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        None
    }
    #[cfg(feature = "solana")]
    fn session_mpp(&self) -> Option<&server::session::SessionMpp> {
        None
    }
    fn fee_payer_wallet(&self) -> Option<&server::telemetry::FeePayerWallet> {
        None
    }
    /// EVM in-flight `(chain_id, from, nonce)` lock used to close the race
    /// between `facilitator.settle` kicking off and the on-chain authorization
    /// state flipping to `true`. The EVM x402 middleware requires
    /// `Some(...)` and will fail closed when the gateway forgot to wire it.
    fn evm_in_flight(&self) -> Option<&server::in_flight::InFlight> {
        None
    }
    /// EVM target chains the gateway advertises in 402 envelopes. Each
    /// `EvmTarget` carries its own recipient/rpc_url/facilitator so the
    /// middleware can dispatch incoming payments to the matching chain.
    ///
    /// When non-empty, the first entry is treated as the *primary* chain
    /// (back-compat with the legacy single-chain `facilitator(&self)`
    /// getter, which still returns this entry's facilitator).
    fn evm_targets(&self) -> &[server::evm_x402_payment::EvmTarget] {
        &[]
    }
}
