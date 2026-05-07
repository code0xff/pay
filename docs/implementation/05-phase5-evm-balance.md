# Phase 5: EVM 잔액 조회

## 목표

`balance.rs`에 EVM 잔액 조회를 추가한다.  
기존 Solana 경로(`get_balances`, `get_stablecoin_balances`)는 변경하지 않는다.

**사용 라이브러리:**
- `alloy = "1.7.3"` (features: `provider-http`, `rpc-types`) — Ethereum JSON-RPC provider

---

## 수정 파일: `rust/crates/core/src/client/balance.rs`

### 5-1. EVM RPC URL 감지 헬퍼

```rust
/// Returns true if the RPC URL looks like an Ethereum JSON-RPC endpoint.
/// Heuristic: checks for well-known EVM RPC hosts.
pub fn is_evm_rpc_url(rpc_url: &str) -> bool {
    let lower = rpc_url.to_lowercase();
    lower.contains("infura.io")
        || lower.contains("alchemy.com")
        || lower.contains("ankr.com/rpc/eth")
        || lower.contains("ankr.com/rpc/base")
        || lower.contains("ankr.com/rpc/optimism")
        || lower.contains("ankr.com/rpc/arbitrum")
        || lower.contains("ankr.com/rpc/sepolia")
        || lower.contains("rpc.sepolia.org")
        || lower.contains("holesky")
        || lower.contains("base-sepolia")
        || lower.contains("evm")
        || lower.contains("ethereum")
        || lower.contains("1rpc.io")
        || lower.contains("publicnode.com")
}
```

### 5-2. EVM 기본 RPC URL 목록

```rust
/// Default EVM RPC URLs by network slug.
/// Override with `PAY_<NETWORK>_RPC_URL` env var (e.g. PAY_SEPOLIA_RPC_URL).
pub fn evm_default_rpc_url(network: &str) -> &'static str {
    match network {
        "ethereum" => "https://ethereum.publicnode.com",
        "base"     => "https://base.publicnode.com",
        "optimism" => "https://optimism.publicnode.com",
        "arbitrum" => "https://arbitrum.publicnode.com",
        "sepolia"  => "https://ethereum-sepolia.publicnode.com",
        "holesky"  => "https://ethereum-holesky.publicnode.com",
        "base-sepolia" => "https://base-sepolia.publicnode.com",
        _          => "https://ethereum.publicnode.com",
    }
}

pub fn evm_rpc_url(network: &str) -> String {
    let env_key = format!("PAY_{}_RPC_URL", network.to_uppercase().replace('-', "_"));
    std::env::var(&env_key).unwrap_or_else(|_| evm_default_rpc_url(network).to_string())
}
```

### 5-3. ERC-20 `balanceOf` ABI 인코딩

alloy `sol!` 매크로로 컴파일 타임에 생성:

```rust
use alloy::sol;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}
```

### 5-4. EVM 잔액 조회 함수

```rust
/// Known EVM stablecoin addresses by (network, symbol).
fn evm_stablecoin_address(network: &str, symbol: &str) -> Option<&'static str> {
    match (network, symbol) {
        // Ethereum mainnet
        ("ethereum", "USDC") => Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
        ("ethereum", "USDT") => Some("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
        // Base mainnet
        ("base", "USDC")     => Some("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
        // Optimism
        ("optimism", "USDC") => Some("0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85"),
        // Arbitrum
        ("arbitrum", "USDC") => Some("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"),
        // Sepolia testnet
        ("sepolia", "USDC")  => Some("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
        _ => None,
    }
}

/// Fetch ETH + known ERC-20 stablecoin balances for an EVM address.
pub async fn get_evm_balances(network: &str, address: &str) -> crate::Result<AccountBalances> {
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::primitives::{Address, U256};
    use std::str::FromStr;

    let rpc_url = evm_rpc_url(network);
    let provider = ProviderBuilder::new()
        .on_http(rpc_url.parse().map_err(|e| {
            crate::Error::Config(format!("Invalid EVM RPC URL `{rpc_url}`: {e}"))
        })?)
        .map_err(|e| crate::Error::Config(format!("EVM provider error: {e}")))?;

    let addr = Address::from_str(address)
        .map_err(|e| crate::Error::Config(format!("Invalid EVM address `{address}`: {e}")))?;

    // ETH balance
    let eth_wei = provider
        .get_balance(addr)
        .await
        .map_err(|e| crate::Error::Config(format!("eth_getBalance failed: {e}")))?;

    // Convert wei to a TokenBalance (18 decimals)
    let eth_ui = eth_wei.to::<u128>() as f64 / 1e18;

    let mut tokens = Vec::new();
    if eth_ui > 0.0 {
        tokens.push(TokenBalance {
            mint: "ETH".to_string(),
            raw_amount: eth_wei.to::<u64>(),
            ui_amount: eth_ui,
            symbol: Some("ETH"),
        });
    }

    // ERC-20 stablecoin balances
    let stablecoins = ["USDC", "USDT"];
    for symbol in stablecoins {
        let Some(contract_addr) = evm_stablecoin_address(network, symbol) else {
            continue;
        };
        let contract = Address::from_str(contract_addr).unwrap();

        let call = IERC20::balanceOfCall { account: addr };
        let result = provider
            .call(&alloy::rpc::types::TransactionRequest {
                to: Some(alloy::primitives::TxKind::Call(contract)),
                input: alloy::rpc::types::TransactionInput::new(
                    alloy::sol_types::SolCall::abi_encode(&call).into(),
                ),
                ..Default::default()
            })
            .await;

        match result {
            Ok(bytes) => {
                let raw = U256::from_be_slice(&bytes);
                let raw_u64 = raw.to::<u64>();
                if raw_u64 > 0 {
                    tokens.push(TokenBalance {
                        mint: contract_addr.to_string(),
                        raw_amount: raw_u64,
                        ui_amount: raw_u64 as f64 / 1_000_000.0, // 6 decimals
                        symbol: Some(symbol),
                    });
                }
            }
            Err(e) => {
                tracing::debug!(
                    symbol = symbol,
                    error = %e,
                    "ERC-20 balanceOf failed — skipping"
                );
            }
        }
    }

    Ok(AccountBalances {
        sol_lamports: 0,  // 구조체 재사용: ETH wei는 TokenBalance로 표현
        tokens,
        tokens_unavailable: false,
    })
}
```

### 5-5. `get_balances()` 체인 감지 분기

기존 함수 시그니처 변경 없이 EVM 분기 추가:

```rust
/// Fetch balances for any supported chain.
///
/// For EVM networks, pass the network slug and address; the RPC URL is
/// resolved from the environment or defaults.
/// For Solana, pass the Solana RPC URL and Base58 pubkey (existing behavior).
pub async fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    if is_evm_rpc_url(rpc_url) {
        // network slug을 RPC URL에서 추론하기 어려우므로 별도 함수 사용 권장.
        // 이 경로는 is_evm_rpc_url()로 감지된 경우의 최선 추론 fallback.
        let network = infer_evm_network(rpc_url);
        return get_evm_balances(&network, pubkey).await;
    }

    // 기존 Solana 경로 (변경 없음)
    let client = balance_client()?;
    // ... 기존 코드 ...
}

fn infer_evm_network(rpc_url: &str) -> String {
    let lower = rpc_url.to_lowercase();
    if lower.contains("sepolia") { return "sepolia".to_string(); }
    if lower.contains("holesky") { return "holesky".to_string(); }
    if lower.contains("base") { return "base".to_string(); }
    if lower.contains("optimism") { return "optimism".to_string(); }
    if lower.contains("arbitrum") { return "arbitrum".to_string(); }
    "ethereum".to_string()
}
```

### 5-6. CLI에서 직접 네트워크 기반 호출 (권장 경로)

CLI `balance` 명령어에서는 네트워크 슬러그를 알고 있으므로 직접 호출:

```rust
// cli/src/commands/balance.rs (수정 예시)
if crate::accounts::is_evm_network_family(&network) {
    let balances = pay_core::client::balance::get_evm_balances(&network, &address).await?;
    render_evm_balances(&balances);
} else {
    let balances = pay_core::client::balance::get_balances(&rpc_url, &address).await?;
    render_solana_balances(&balances);
}
```

---

## 의존성 추가 확인

`alloy`는 Phase 1에서 이미 추가되었으나 `provider-http` feature가 포함되어 있는지 확인:

```toml
# rust/Cargo.toml (workspace)
alloy = { version = "1.7.3", features = [
    "signer-local",
    "provider-http",   # ← get_evm_balances에 필요
    "eip712",
    "sol-types",       # ← sol! 매크로에 필요
    "rpc-types",       # ← TransactionRequest에 필요
] }
```

---

## 검증

```bash
# 빌드
cargo build -p pay-core

# 단위 테스트
cargo test -p pay-core balance

# 예상 통과 테스트
# client::balance::tests::is_evm_rpc_url_detects_sepolia
# client::balance::tests::evm_default_rpc_url_returns_known_endpoints

# 통합 테스트 (실제 RPC 호출 — 인터넷 필요)
# PAY_SEPOLIA_RPC_URL=https://ethereum-sepolia.publicnode.com \
# cargo test -p pay-core evm_balance_sepolia -- --ignored
```

### 수동 검증

```bash
# Sepolia 잔액 조회
pay --network sepolia balance

# 기존 Solana 잔액 조회 — 회귀 없음 확인
pay balance
pay --network devnet balance
```

---

## 다음 단계

Phase 1–5 구현 완료.  
전체 통합 테스트:

```bash
cargo test -p pay-core
cargo build -p pay-cli
pay --network sepolia curl https://<x402-evm-endpoint>
```
