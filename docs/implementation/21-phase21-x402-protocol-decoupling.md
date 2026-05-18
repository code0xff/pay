# Phase 21 — x402 Protocol Type Decoupling

## 목표

`solana_x402` SDK 의존을 EVM/x402 코드 경로에서 완전히 제거한다.
결과: `cargo build` (no features) 가 EVM-only 바이너리로 빌드되고,
`solana-*` 의존성이 빌드 그래프에서 사라진다.

---

## 동기

Phase 20 진행 중 발견된 아키텍처 이슈:

`solana_x402` 크레이트는 단순 Solana SDK가 아니라 **x402 프로토콜의 wire
타입과 상수까지 정의**한다. 그래서 EVM x402 코드 경로(`client/evm.rs`,
`client/x402.rs`, `runner.rs`, `skills/probe.rs`)도 다음을 import해서
사용한다:

- `solana_x402::exact::PaymentRequirements` — wire 타입
- `solana_x402::exact::PaymentRequiredEnvelope` — wire 타입
- `solana_x402::{X402_VERSION_V1, V2, X402_VERSION_FIELD, ...}` — 상수
- `solana_x402::client::exact::parse_x402_challenge_for_network` — 파서

Phase 20만으로는 EVM-only 빌드를 만들 수 없는 이유는 바로 이 의존성
때문이다. Phase 21이 이를 분리한다.

---

## 진행 상태

### 완료 (Phase 21-1)

- ✅ `pay-core::x402_proto` 모듈 신규 생성 (510 줄)
  - `PaymentRequirements` 구조체 + custom Serialize/Deserialize
  - `PaymentRequiredEnvelope` 구조체
  - `ResourceInfo` 구조체
  - 15개 프로토콜 상수 (헤더, 버전, scheme, CAIP-2 Solana 클러스터)
  - `parse_x402_challenge_for_network` 파서
  - 헬퍼 함수들 (`normalize_network_identifier`, `caip2_network_for_cluster` 등)
  - 단위 테스트 (round-trip 검증)
- ✅ `lib.rs`에서 `pub mod x402_proto;` 노출
- ✅ `cargo check --features solana` 통과 (1.02s)

### 잔여 작업

#### Phase 21-2: `client/x402.rs` 마이그레이션 (가장 큰 작업)

**도전 과제**:
- 1429 줄, 70개 `solana_x402` 참조
- `Challenge::siwx: Option<SiwxExtension>` 필드가 `solana_x402::siwx::SiwxExtension` 타입
- `Challenge::requirements`, `all_accepts`는 PaymentRequirements 타입 — 우리 타입으로 교체 시
  `siwx_extension_from_payment_required(&envelope)` 등 SDK 함수가 타입 충돌

**해결 방안 (두 가지 검토)**:

옵션 A — Challenge 구조체에 우리 타입 사용 + SDK 호출 시 변환
```rust
use crate::x402_proto::{PaymentRequirements, PaymentRequiredEnvelope};

#[cfg(feature = "solana")]
fn to_solana_envelope(local: &PaymentRequiredEnvelope) -> solana_x402::exact::PaymentRequiredEnvelope {
    // serde round-trip 또는 수동 필드 매핑
}
```

옵션 B — Challenge 구조체에 우리 타입, siwx 필드만 feature-gate
```rust
pub struct Challenge {
    pub x402_version: u64,
    pub requirements: crate::x402_proto::PaymentRequirements,
    pub all_accepts: Vec<crate::x402_proto::PaymentRequirements>,
    #[cfg(feature = "solana")]
    pub siwx: Option<solana_x402::siwx::SiwxExtension>,
}
```

**옵션 B를 권장** — siwx 필드 자체가 Solana 한정 기능이므로 feature-gate
하는 것이 자연스럽다. SDK 호출 시점에 PaymentRequiredEnvelope를 변환하는
헬퍼만 추가하면 된다.

**세부 작업**:
- [ ] Imports 분리: 공통(x402_proto) vs solana 전용(SolanaSigner, RpcClient, siwx, build_payment_header*)
- [ ] `Challenge::siwx` 필드 `#[cfg(feature = "solana")]` 게이트
- [ ] `SiwxAuthChallenge` 구조체 전체 `#[cfg(feature = "solana")]` 게이트
- [ ] `parse_siwx_auth()` 함수 게이트
- [ ] `build_payment()` Solana 분기를 별도 함수로 분리 + `#[cfg(feature = "solana")]` 게이트
- [ ] `build_siwx_header()`, `sign_in_with_x()` 게이트
- [ ] SDK 호출 직전에 `crate::x402_proto::PaymentRequirements` →
      `solana_x402::exact::PaymentRequirements` 변환 헬퍼 추가
- [ ] EVM-only 빌드에서 `parse()` 함수가 EVM accepts만 보고도 동작하는지 확인
- [ ] 테스트 케이스: solana 한정 테스트는 cfg 게이트, 나머지는 양쪽 빌드 통과

#### Phase 21-3: `client/evm.rs` 마이그레이션

- [ ] `use solana_x402::exact::PaymentRequirements` → `use crate::x402_proto::PaymentRequirements`
- [ ] `use solana_x402::{X402_VERSION_V1, X402_VERSION_V2}` → `use crate::x402_proto::{...}`
- [ ] 테스트 import도 동일하게 변경

#### Phase 21-4: `runner.rs` + `skills/probe.rs` 마이그레이션

- [ ] `runner.rs:425` — `solana_x402::PAYMENT_REQUIRED_HEADER` → `crate::x402_proto::PAYMENT_REQUIRED_HEADER`
- [ ] `runner.rs` MPP/Session arm을 `#[cfg(feature = "solana")]` 게이트 (Phase 20-step3 잔여 작업)
- [ ] `runner.rs::classify_402` 디스패치 순서 x402-first로 재정렬 (설계 doc 명시)
- [ ] `skills/probe.rs` — `solana_x402::exact::PaymentRequirements` 4곳 → `crate::x402_proto::PaymentRequirements`
- [ ] `skills/probe.rs` — `solana_mpp::ChargeRequest` 참조 게이트 (392, 396, 596 라인)

#### Phase 21-5: `signer.rs` 게이트

- [ ] ed25519/Solana keypair 로딩 코드를 `#[cfg(feature = "solana")]` 블록으로 감싸기
- [ ] EVM signer 분기 (secp256k1)는 unconditional 유지
- [ ] `load_signer_for_network_payment_with_intent`의 Solana 분기 게이트

#### Phase 21-6: CLI Step 4 + 최종 검증

- [ ] `commands/account/*` (new, import, list, destroy, export, edit): Solana 분기 게이트
- [ ] `commands/send.rs`, `topup.rs`, `whoami.rs`: Solana 분기 게이트
- [ ] `commands/server/start.rs`: MPP 서버 시작 분기 게이트
- [ ] `commands/skills/*`: Solana probe/build 분기 게이트
- [ ] `network.rs`, `components/account.rs`, `components/link.rs`: Solana 헬퍼 게이트
- [ ] `cargo check` (default = []) — 통과 확인 (현재 27개 에러)
- [ ] `cargo check --features solana` — 통과 확인
- [ ] `cargo test` — EVM-only 테스트 통과
- [ ] `cargo test --features solana` — 전체 테스트 통과
- [ ] `cargo tree -p pay-core | grep -c solana` → 0 (no features 빌드)

---

## 사전 조사 결과 (Phase 21-1 단계에서 확인됨)

1. `solana_x402::exact::PaymentRequirements`의 15개 필드와 serde 동작이
   `x402_proto::PaymentRequirements`에 1:1로 복제되었다.
2. v1/v2 wire 포맷의 필드명 차이(`payTo`/`recipient`, `asset`/`currency`,
   `maxAmountRequired`/`amount`, `maxTimeoutSeconds`/`maxAge`)는 양쪽
   Deserialize impl이 동일하게 처리한다.
3. `parse_x402_challenge_for_network` 파서는 헤더 base64 디코드 → JSON
   파싱 → preferred network 매칭으로 동작. Solana SDK에 의존하지 않는다.
4. `solana_x402` SDK는 동일 패키지에서 wire 타입과 Solana 전용 기능
   (SIWX, MemorySigner, RpcClient)을 함께 export — Cargo feature로
   세분화되어 있지 않다. 우리가 wire 타입을 자체 정의하는 것 외에는
   해법이 없다.

---

## 완료 기준

- [ ] `cargo build` (no features) — 통과, solana-* 의존성 0개
- [ ] `cargo build --features solana` — 통과, 기존 동작 유지
- [ ] `cargo test` 양쪽 모두 통과
- [ ] `cargo tree -p pay-core | grep solana | wc -l` → 0 (no features)
- [ ] CLAUDE.md `결제 프로토콜` 섹션 업데이트 (Phase 20 설계 doc 참조)
