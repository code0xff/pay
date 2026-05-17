//! Minimal EVM x402 gateway runtime.
//!
//! `start::StartCommand::run` dispatches here when the API's operator config
//! selects `protocol: x402` with an EVM network slug. The runtime is
//! intentionally slim: no PDB, no OpenAPI route rewriting, no Solana RPC
//! browser-proxy, no sandbox auto-funding. Those features all assume Solana
//! validators / keypairs and would need EVM-specific rewrites to be useful
//! here. The current goal is "a YAML toggle launches a working EVM x402 402
//! gateway"; richer UX can layer on once the wire path is stable.

use std::sync::Arc;

use axum::middleware;
use axum::routing::{any, get};
use owo_colors::OwoColorize;
use pay_core::PaymentState;
use pay_core::server::evm_x402_payment::EvmTarget;
use pay_core::server::in_flight::InFlight;
use pay_core::server::session::SessionMpp;
use pay_core::server::telemetry::FeePayerWallet;
use pay_core::server::x402_facilitator::FacilitatorClient;
use pay_core::solana_x402::server::X402;
use pay_types::metering::ApiSpec;
use solana_mpp::server::Mpp;

#[derive(Clone)]
struct EvmAppState {
    apis: Arc<Vec<ApiSpec>>,
    /// Phase 17: pre-validated EVM target table built at boot from
    /// `operator.network` + `operator.extra_evm_networks`. The middleware
    /// iterates this list to compose `accepts[]` and to dispatch incoming
    /// payments by chain_id. The legacy `facilitator()` accessor returns
    /// the first target's facilitator for back-compat with older state
    /// consumers.
    targets: Arc<Vec<EvmTarget>>,
    /// Per-node in-flight `(chain_id, from, nonce)` lock; the EVM x402
    /// middleware uses it to close the race window between facilitator
    /// settlement and on-chain mining. Naturally bounded by the number of
    /// concurrent payments, so no LRU eviction is required.
    in_flight: Arc<InFlight>,
}

impl PaymentState for EvmAppState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        None
    }
    fn mpps(&self) -> Vec<&Mpp> {
        Vec::new()
    }
    fn x402s(&self) -> Vec<&X402> {
        Vec::new()
    }
    fn facilitator(&self) -> Option<&FacilitatorClient> {
        self.targets.first().map(|t| t.facilitator.as_ref())
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        None
    }
    fn session_mpp(&self) -> Option<&SessionMpp> {
        None
    }
    fn fee_payer_wallet(&self) -> Option<&FeePayerWallet> {
        None
    }
    fn evm_in_flight(&self) -> Option<&InFlight> {
        Some(&self.in_flight)
    }
    fn evm_targets(&self) -> &[EvmTarget] {
        &self.targets
    }
}

/// Block-on entry called from `StartCommand::run` once it has detected
/// `operator.protocol = x402` + EVM `operator.network`. Validates the
/// operator block, builds a minimal Axum app wired to
/// `evm_x402_payment_middleware`, and serves until the process exits.
pub fn run(bind: &str, api: ApiSpec) -> pay_core::Result<()> {
    let operator = api.operator.as_ref().ok_or_else(|| {
        pay_core::Error::Config(
            "EVM x402 mode requires an `operator` block in the YAML spec".to_string(),
        )
    })?;
    // Phase 17: resolve every advertised EVM chain (primary + extras) up
    // front. `resolve_evm_targets` enforces recipient/rpc/facilitator
    // presence and rejects duplicate chain ids, so a misconfigured YAML
    // fails here rather than on the first paid request.
    let targets = pay_core::server::evm_x402_payment::resolve_evm_targets(operator)
        .map_err(pay_core::Error::Config)?;
    if targets.is_empty() {
        return Err(pay_core::Error::Config(
            "EVM x402 mode requires at least one EVM target chain (operator.network or operator.extra_evm_networks)"
                .to_string(),
        ));
    }
    let in_flight = Arc::new(InFlight::new());

    let bind = bind.to_string();
    let api_for_router = api.clone();
    let state = EvmAppState {
        apis: Arc::new(vec![api_for_router]),
        targets: Arc::new(targets),
        in_flight,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("Failed to build tokio runtime: {e}")))?;

    rt.block_on(async move {
        print_banner(&state.targets, &state.apis[0]);

        let api_for_fallback = state.apis[0].clone();
        let mut app: axum::Router<EvmAppState> = axum::Router::new()
            .route("/__402/health", get(|| async { "ok" }))
            .fallback(any(move |req: axum::http::Request<axum::body::Body>| {
                let api = api_for_fallback.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let path = parts.uri.path().trim_start_matches('/');
                    if pay_core::server::metering::find_endpoint_by_path(&api, path).is_none() {
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({"error": "not_found"})),
                        ));
                    }
                    let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    pay_core::server::proxy::forward_request(
                        &api,
                        parts.method,
                        &parts.uri,
                        &parts.headers,
                        bytes,
                    )
                    .await
                    .unwrap_or_else(|e| e)
                }
            }));
        app = app.layer(middleware::from_fn_with_state(
            state.clone(),
            pay_core::server::evm_x402_payment::evm_x402_payment_middleware::<EvmAppState>,
        ));
        let app = app.with_state(state);

        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|e| pay_core::Error::Config(format!("Failed to bind {bind}: {e}")))?;

        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| bind.clone());
        eprintln!(
            "{} EVM x402 gateway listening on http://{}",
            "ready".green(),
            local
        );

        axum::serve(listener, app)
            .await
            .map_err(|e| pay_core::Error::Config(format!("Server error: {e}")))?;
        Ok::<(), pay_core::Error>(())
    })
}

fn print_banner(targets: &[EvmTarget], api: &ApiSpec) {
    let banner = crate::components::render_pay_banner(crate::components::PAY_SH_TAGLINE.dimmed());
    if !banner.is_empty() {
        eprintln!("{banner}");
        eprintln!();
    }
    let chain_list = targets
        .iter()
        .map(|t| format!("{} (chain_id={})", t.network, t.chain_id))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("{}\t{} (EVM x402)", "networks".dimmed(), chain_list.green());
    for target in targets {
        eprintln!(
            "{}\t{} → {} via {}",
            target.network.dimmed(),
            target.recipient,
            target.rpc_url,
            target.facilitator.base_url(),
        );
    }
    eprintln!();
    let metered = api
        .endpoints
        .iter()
        .filter(|e| e.metering.is_some())
        .count();
    let free = api.endpoints.len() - metered;
    eprintln!(
        "{}",
        format!(
            "{} endpoints ({} metered, {} free)",
            api.endpoints.len(),
            metered,
            free
        )
        .dimmed()
    );
    eprintln!();
}
