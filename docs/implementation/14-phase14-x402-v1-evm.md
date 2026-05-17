# Phase 14: x402 v1 EVM 지원

## 배경

Phase 13 개발 중 v1+EVM 조합을 명시 거절하는 코드를 작성했으나, 이를 보류했다.

`x402-chain-eip155` 크레이트는 x402 EVM의 **공식** SDK가 아니며, 해당 크레이트가
`V2Eip155ExactClient`만 제공한다는 사실이 x402 v1 스펙에서 EVM을 지원하지 않는다는
의미가 아니다. x402 v1과 v2의 차이는 주로 헤더 이름(`X-Payment` vs
`PAYMENT-SIGNATURE`)과 envelope 포맷에 있으며, 체인 지원 여부는 별개다.

x402 v1 EVM 서버가 실제로 존재할 수 있고, 현재 `pay` 클라이언트는 해당 서버에
대해 Solana 서명을 시도하다 facilitator에서 `invalid signature`로 거절당하는
상황이다. 사용자는 원인을 알 수 없다.

---

## 목표

x402 v1 헤더(`X-Payment`)를 사용하는 EVM 서버에 대해 올바른 EVM 서명을 생성한다.

---

## 사전 조사 항목

구현 전 다음을 확인해야 한다.

### 1. x402 v1 EVM envelope 포맷

x402 v1 스펙에서 EVM 결제 payload가 어떤 구조인지 확인한다.

- x402 공식 문서 또는 Coinbase TypeScript SDK 소스 참조
  - 저장소: `https://github.com/coinbase/x402`
- v1 EVM payload가 v2와 동일한 EIP-712 구조를 사용하는지, 아니면 다른 서명 스킴을 쓰는지 확인

### 2. `x402-chain-eip155` 크레이트 v1 지원 여부

- `x402-chain-eip155` 크레이트가 v1 EVM 클라이언트를 제공하는지 확인
  (`V1Eip155ExactClient` 또는 유사 타입 존재 여부)
- 미제공 시: 컨트리뷰션 가능 여부 검토

### 3. x402 Rust SDK 공식 여부

- x402 생태계에서 공식 Rust SDK가 존재하는지 확인
- `x402-chain-eip155`가 커뮤니티 크레이트라면, 공식 SDK 마이그레이션 가능성도 검토

---

## 구현 옵션

사전 조사 결과에 따라 아래 중 하나를 선택한다.

### A. `x402-chain-eip155`에 v1 EVM 지원 기여

v1 EVM 스펙이 확인되고 크레이트가 컨트리뷰션을 받는다면:
- `V1Eip155ExactClient` 구현 PR 제출
- 해당 크레이트 업스트림 후 `pay-core`에서 사용

### B. `pay-core` 내 직접 구현

라이브러리 컨트리뷰션이 어렵거나 느린 경우:
- `crate::client::evm::sign_evm_payment_v1` 추가
- `build_payment` 에서 `challenge.x402_version == X402_VERSION_V1` 분기에서 호출
- v1 envelope 포맷에 맞게 서명 생성

### C. x402 Rust SDK 전면 교체

공식 Rust SDK가 있다면:
- `x402-chain-eip155` 의존성을 공식 SDK로 교체
- v1/v2 모두 공식 구현 사용

---

## 변경 파일 (예상)

| 파일 | 변경 |
|------|------|
| `crates/core/src/client/evm.rs` | `build_evm_payment` — v1 분기 추가 |
| `crates/core/src/server/evm_x402_payment.rs` | v1 헤더 수신 시 v1 EVM 검증 처리 |
| `rust/Cargo.toml` | x402 Rust SDK 교체 시 의존성 변경 |

---

## 우선순위

~~P3~~ — Phase 19 시점에 구현 완료.

## 구현 결과

`x402-chain-eip155 1.4.6` 의 `V1Eip155ExactClient` 가 v2 클라이언트와
동일한 인터페이스를 제공한다는 사실이 사전조사에서 확인되어, **옵션 A
(공식 SDK 그대로 사용)** 로 진행했다.

`client::evm::sign_evm_payment` 에 `version: u64` 인자를 추가하고
`X402_VERSION_V1` 이면 `V1Eip155ExactClient` + `v1::PaymentRequired`
envelope 으로 분기. `build_evm_payment` 의 결과 header 도 `X402_V1_PAYMENT_HEADER`
(`X-Payment`) 로 바뀐다.

v1 envelope 은 v2 와 두 가지가 다르다:
1. `network` 필드가 CAIP-2 (`eip155:8453`) 가 아니라 SDK 의 short name
   (`base`) 이어야 한다 — `ChainId::as_network_name()` 으로 변환.
2. `amount` 가 아니라 `maxAmountRequired` 이고, `resource`/`description`
   필드를 envelope 안에 직접 가져야 한다.

이 변환은 `v1_envelope_reshape` 헬퍼로 캡슐화되어 있어 v2 경로에는 영향
없음.

테스트: `build_evm_payment_v1_emits_x_payment_header` (base-sepolia 기준
— Ethereum Sepolia 는 x402-types short name 테이블에 없음),
`build_evm_payment_rejects_unknown_x402_version`.
