# x402 Ethereum 멀티체인 구현 — 개요

## 목표

x402 프로토콜에서 Solana와 Ethereum(EVM)을 동시 지원한다.  
서버가 `accepts: [solana:..., eip155:1, ...]`로 여러 체인을 제안하면, 클라이언트가 구성된 지갑에 맞는 체인을 선택해 서명한다.

**변경하지 않는 것**: MPP, Session, 서버 사이드 코드 — 모두 Solana 전용으로 유지.

> **MPP EVM 가능성 검토 (2026-05-07)**  
> `draft-evm-charge-00` 스펙이 [paymentauth.org](https://paymentauth.org)에 공개되어 있어 EVM MPP가 프로토콜 레벨에서는 가능하다.  
> 그러나 Rust SDK가 없고, Session intent의 EVM 스펙도 미존재한다.  
> x402가 동일한 EVM 멀티체인 요구사항을 훨씬 낮은 비용으로 충족하므로 **EVM MPP는 현 단계에서 보류**한다.

---

## 구현 단계

| Phase | 파일 | 내용 | 의존 |
|-------|------|------|------|
| **1** | `chain.rs` (신규) | ChainFamily/ChainSigner 추상화 | alloy |
| **2** | `accounts.rs` | EVM 계정 레지스트리 확장 | alloy-signer-local |
| **3** | `x402.rs` + `evm.rs` (신규) | x402 멀티체인 파싱·서명 | x402-chain-eip155, alloy |
| **4** | `runner.rs` | EVM 거부 코드 제거, store 전달 | — |
| **5** | `balance.rs` | EVM 잔액 조회 | alloy-provider |

---

## Cargo feature flag: `evm` (opt-in)

EVM 지원은 **`evm` feature flag 뒤에 분리**되어 있으며 기본값은 **off**다. Solana-only 사용자는 alloy / x402-chain-eip155 의존성 비용을 부담하지 않는다.

- **Feature 정의**: `rust/crates/core/Cargo.toml`의 `[features]` 블록 (`evm = ["dep:alloy", "dep:x402-chain-eip155", "dep:hex"]`)
- **CLI 재노출**: `rust/crates/cli/Cargo.toml`의 `evm = ["pay-core/evm"]`
- **활성화 방법**: `cargo build --features evm` (워크스페이스 빌드 시 `--features pay/evm`)
- **검증**: `cargo tree -p pay-core | grep -c alloy` → 반드시 `0` (기본 빌드)

Phase 1~5는 모두 동일한 `evm` flag 아래 단계적으로 진행하여, 반쯤 구현된 EVM 스택이 비활성 빌드에 노출되지 않는다. Phase별 feature 상호작용은 각 phase 문서의 `### N.0 Feature flag interaction` 섹션 참조.

비활성 빌드에서 EVM 네트워크 슬러그를 입력하면 다음 에러를 반환한다:
```
Network `ethereum` requires EVM support, but this `pay` build does not include
it. Rebuild with `cargo build --features evm`.
```

## 추가 의존성 (rust/Cargo.toml workspace)

```toml
# EVM x402 — Coinbase 공식 크레이트
x402-chain-eip155 = "1.4.4"

# Ethereum 공식 Rust 라이브러리
alloy = { version = "1.7.3", features = [
    "signer-local",   # PrivateKeySigner (secp256k1, pure Rust)
    "provider-http",  # HTTP JSON-RPC provider
    "eip712",         # EIP-712 structured data signing
    "sol-types",      # compile-time ABI/EIP-712 타입
    "rpc-types",      # Ethereum RPC 타입
] }

# EVM 키 hex 인코딩 (EvmChainSigner::to_hex_key)
hex = "0.4"
```

`pay-core/Cargo.toml`에는 `optional = true`로 추가하여 `evm` feature가 켜질 때만 빌드 그래프에 포함시킨다:
```toml
alloy             = { workspace = true, optional = true }
x402-chain-eip155 = { workspace = true, optional = true }
hex               = { workspace = true, optional = true }
```

---

## EVM x402 결제 흐름 (구현 후)

```
서버 → 402 Payment-Required
  accepts: [
    { network: "solana:5eykt4...", asset: "USDC_MINT", payTo: "7xK..." },
    { network: "eip155:8453",     asset: "0xUSdc...",  payTo: "0x..." }
  ]

클라이언트 (pay CLI)
  1. parse_x402_challenge() — accepts 배열 전체 파싱
  2. select_best_chain()    — accounts.yml 기반 체인 선택
  3. build_payment()        — 체인별 디스패치
     ├── Solana → build_solana_payment() (기존)
     └── EVM   → build_evm_payment()    (신규)
           1. alloy PrivateKeySigner로 ERC-3009 서명
           2. PAYMENT-SIGNATURE 헤더 반환
  4. 재시도 with PAYMENT-SIGNATURE
```

---

## 파일별 구현 문서

- [Phase 1: Chain 추상화](./01-phase1-chain-abstraction.md)
- [Phase 2: EVM 계정 레지스트리](./02-phase2-account-registry.md)
- [Phase 3: x402 멀티체인](./03-phase3-x402-multichain.md)
- [Phase 4: Runner 정리](./04-phase4-runner-cleanup.md)
- [Phase 5: EVM 잔액 조회](./05-phase5-evm-balance.md)
