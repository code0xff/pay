# Phase 8: EVM 라이브 통합 테스트

## 목표

Phase 5에서 도입된 `get_evm_balances`가 실제 EVM 노드 응답에 대해 정상 동작
하는지 검증한다. 단위 테스트는 입출력 변환·URL 라우팅 수준만 다루고 있어,
JSON-RPC 응답 디코딩과 ERC-20 `balanceOf` 인코딩의 회귀를 잡지 못한다.

**원칙**

1. 통합 테스트는 기존 `network_tests` feature 플래그로 게이팅한다(이미
   `openapi_resolve_smoke`에서 사용 중인 패턴).
2. 기본 빌드(`cargo test`)에서는 절대 네트워크에 접근하지 않는다.
3. 외부 RPC는 환경변수로 오버라이드 가능해야 한다.

---

## 8-1. 크레이트 구조

`crates/core/tests/evm_balance_tests.rs` 신규.

```rust
//! Live Sepolia/Base-Sepolia integration tests for `get_evm_balances`.
//!
//! Gated under `evm` + `network_tests` features so CI defaults don't
//! hit publicnode RPC. Run with:
//!
//! ```
//! cargo test -p pay-core --features evm,network_tests --test evm_balance_tests
//! ```
//!
//! Optional env overrides:
//! - `PAY_SEPOLIA_RPC_URL`       — alternate Sepolia endpoint
//! - `PAY_BASE_SEPOLIA_RPC_URL`  — alternate Base-Sepolia endpoint
//! - `PAY_EVM_TEST_ADDRESS`      — funded test wallet address (defaults below)

#![cfg(all(feature = "evm", feature = "network_tests"))]

use pay_core::client::balance::get_evm_balances;
```

### 의존성 게이팅

`crates/core/Cargo.toml`에는 이미 `network_tests` feature가 있다(Phase 0
시점부터). `evm`+`network_tests` 동시 활성화 시에만 모듈이 컴파일된다.

```toml
[features]
network_tests = []
evm = ["dep:alloy", "dep:x402-chain-eip155", ...]
```

`Cargo.toml`의 `[[test]]` 섹션에 별도 등록은 필요 없다. cargo가 자동 발견.

---

## 8-2. 테스트 케이스

### 8-2-1. 빈 주소 — Sepolia

```rust
#[tokio::test]
async fn sepolia_burn_address_returns_zero_or_minimal_balance() {
    let burn = "0x000000000000000000000000000000000000dEaD";
    let balances = get_evm_balances("sepolia", burn).await.expect("RPC ok");

    // Burn 주소에는 잔액이 거의 없거나 USDC만 가끔 들어옴.
    // 0 잔액 → tokens가 빈 벡터.
    // 잔액 0 초과 → ui_amount 양수.
    for token in &balances.tokens {
        assert!(
            token.ui_amount > 0.0,
            "tokens 리스트에 들어왔다면 양수여야 함: {:?}",
            token
        );
        assert!(matches!(token.symbol, Some("ETH") | Some("USDC") | Some("USDT")));
    }
    assert_eq!(balances.sol_lamports, 0, "EVM은 SOL 잔액이 없음");
    assert!(!balances.tokens_unavailable);
}
```

### 8-2-2. 잘못된 주소 — 명확한 에러

```rust
#[tokio::test]
async fn sepolia_invalid_address_returns_clear_error() {
    let err = get_evm_balances("sepolia", "not-a-hex-address")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Invalid EVM address"));
}
```

### 8-2-3. ETH 단위 변환 정확도

```rust
#[tokio::test]
async fn sepolia_eth_balance_uses_18_decimals() {
    // 알려진 양수 잔액을 가진 테스트 주소 (env로 오버라이드 가능).
    let addr = std::env::var("PAY_EVM_TEST_ADDRESS")
        .unwrap_or_else(|_| "0x000000000000000000000000000000000000dEaD".to_string());

    let balances = get_evm_balances("sepolia", &addr).await.expect("RPC ok");

    if let Some(eth) = balances.tokens.iter().find(|t| t.symbol == Some("ETH")) {
        // 단위 표기 검증: 1 ETH = 1e18 wei. 표시값이 wei가 아닌 ETH인지.
        // 통상 burn 주소도 1 ETH 미만 ~ 수십 ETH 범위.
        assert!(eth.ui_amount > 0.0);
        assert!(eth.ui_amount < 1_000_000.0, "wei가 노출됐을 수 있음: {}", eth.ui_amount);
    }
}
```

### 8-2-4. ERC-20 USDC 호출 (선택)

USDC 컨트랙트가 Sepolia에 deployment되어 있으므로, 잘 알려진 funded faucet
주소로 호출. 실패해도 무시(faucet 가치는 변동).

```rust
#[tokio::test]
async fn sepolia_usdc_balanceof_does_not_panic() {
    let addr = "0x000000000000000000000000000000000000dEaD";
    // 호출 자체가 panic/타임아웃 없이 끝나면 충분. 잔액은 0일 수 있음.
    let _ = get_evm_balances("sepolia", addr).await.expect("RPC ok");
}
```

### 8-2-5. Base-Sepolia 스모크

```rust
#[tokio::test]
async fn base_sepolia_burn_address_smoke() {
    let burn = "0x000000000000000000000000000000000000dEaD";
    let balances = get_evm_balances("base-sepolia", burn).await.expect("RPC ok");
    assert!(!balances.tokens_unavailable);
}
```

---

## 8-3. CI 정책

### 기본 실행 (PR 단위)

```bash
cargo test --features evm,server --workspace
```

→ `evm_balance_tests`는 `network_tests`가 꺼져 있어 컴파일조차 되지 않음 (정상).

### 야간/수동 실행

`.github/workflows/evm-network-tests.yml` (신규, 권장):

```yaml
name: EVM network tests
on:
  schedule:
    - cron: "0 6 * * *"   # 매일 06:00 UTC
  workflow_dispatch:

jobs:
  evm-network-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - run: cargo test -p pay-core --features evm,network_tests --test evm_balance_tests
        env:
          PAY_SEPOLIA_RPC_URL: ${{ secrets.SEPOLIA_RPC_URL }}
          PAY_BASE_SEPOLIA_RPC_URL: ${{ secrets.BASE_SEPOLIA_RPC_URL }}
```

publicnode 무료 RPC는 rate limit가 있어 매일 한 번이면 충분. 우선 cron 없이
`workflow_dispatch`만 등록하고 안정화 후 cron 추가.

### 로컬 실행

```bash
cargo test -p pay-core --features evm,network_tests --test evm_balance_tests -- --nocapture
```

---

## 8-4. 단위 테스트와의 경계

| 검증 항목 | 단위 테스트 (Phase 5) | 통합 테스트 (Phase 8) |
|---|---|---|
| `evm_default_rpc_url` 매핑 | ✅ | — |
| `evm_stablecoin_address` 매핑 | ✅ | — |
| `u256_saturate_u64` saturation | ✅ | — |
| `evm_rpc_url` 환경변수 처리 | ✅ | — |
| JSON-RPC 응답 디코딩 | — | ✅ |
| ERC-20 `balanceOf` ABI 인코딩/디코딩 | — | ✅ |
| `format_units` 단위 변환 정확성 | — | ✅ |
| 네트워크 타임아웃/에러 매핑 | — | ✅ |

---

## 검증

```bash
# 1. 기본 빌드/테스트 — network 의존 0
cargo test --features evm,server --workspace
# 예상: evm_balance_tests 컴파일 안 됨, 다른 모든 테스트 통과

# 2. 명시적 통합 실행
cargo test -p pay-core --features evm,network_tests --test evm_balance_tests
# 예상: sepolia/base-sepolia RPC 응답 받고 5개 테스트 통과
```

---

## 다음 단계

[Phase 9: EVM 키스토어 백엔드](./09-phase9-evm-keystore.md)
