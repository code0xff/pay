# Phase 20 — `solana` Feature Gate (EVM/x402 First)

## 목표

EVM + x402를 기본(unconditional) 컴파일 대상으로 하고,
Solana / MPP / Session을 `solana` Cargo feature로 opt-in화한다.

```
# 기본 빌드 (EVM only)
cargo build

# Solana 포함 빌드 (기존 동작 유지)
cargo build --features solana
```

---

## 동기

- EVM/x402가 주요 결제 경로로 자리잡음 (Phase 1–19 완료)
- Solana 의존성(solana-mpp, solana-x402, ed25519-dalek, bs58 등 9개 크레이트)이
  EVM-only 빌드에 불필요하게 포함됨
- 바이너리 크기·빌드 시간 최적화, CI/CD EVM-only 경로 단순화

---

## 프로토콜 정책 (x402-first)

Phase 20은 단순 feature gate 재편이 아니라 **프로토콜 계층의 위계 재정의**다.

### 우선순위 명문화

| 프로토콜 | 위계 | 컴파일 | 비고 |
|---------|-----|--------|------|
| **x402** | **1st-class** (primary) | 무조건 | EVM 기본, Solana는 feature 활성 시 추가 |
| **MPP** | 2nd-class (Solana legacy) | `solana` opt-in | Solana 전용. EVM `draft-evm-charge-00`은 보류 |
| **Session** | 2nd-class (Solana legacy) | `solana` opt-in | Solana 전용 |

### 코드 레벨에 적용되는 정책

1. **`RunOutcome` enum 의미 분리**
   - `X402Challenge` — 항상 존재 (primary path)
   - `MppChallenge` / `SessionChallenge` — `#[cfg(feature = "solana")]`로 게이트
     (Solana 비활성 빌드에서는 enum 차원에서 존재하지 않음)

2. **`classify_402` 디스패치 순서 재정렬**
   - **Before (current)**: MPP 헤더 먼저 체크 → x402 헤더 체크 (역사적 이유)
   - **After (Phase 20)**: x402 헤더 먼저 체크 → Solana feature 활성 시 MPP/Session fallback
   - 변경 이유: x402가 primary protocol임을 코드 순서로도 명시

3. **EVM-first 라우팅 (이미 적용된 정책 재확인)**
   - Phase 16 — `select_best_chain`이 EVM 엔트리 우선 선택
   - Phase 15 — `classify_outcome` EVM-first
   - Phase 20 — 위 정책을 *기본 빌드에서는 강제*화
     (Solana 분기 자체가 컴파일되지 않으므로 EVM 외 선택지 없음)

4. **에러 메시지 톤**
   - Solana-비활성 빌드에서 MPP/Session 헤더 수신 시:
     ```
     received MPP-style 402 challenge (method="solana"), but this build
     does not include Solana support. Rebuild with --features solana,
     or ask the gateway operator for an x402 endpoint.
     ```
   - Solana-비활성 빌드에서 Solana 네트워크 슬러그 입력 시 (Phase 4 패턴):
     ```
     Network 'mainnet' requires Solana support.
     Rebuild with --features solana.
     ```

5. **`accounts.yml` 마이그레이션 정책**
   - Solana 계정 항목이 있는 YAML을 Solana-비활성 빌드로 로드 시:
     - **무시하고 진행** (graceful) — EVM 계정만 사용
     - WARN 로그 1회: `"Skipping Solana account 'mainnet' — built without --features solana"`
   - 강제 실패시키지 않는 이유: 사용자가 멀티체인 YAML을 그대로 두고 EVM-only 바이너리만 갈아끼울 수 있어야 함

### CLAUDE.md 결제 프로토콜 표 업데이트

기존:
```
| MPP  | Solana 전용 |
| x402 | Solana + Ethereum |
| Session | Solana 전용 |
```

Phase 20 이후:
```
| x402 | **primary** — EVM 무조건, Solana는 `solana` feature 활성 시 |
| MPP  | secondary — `solana` feature 한정 (Solana 전용 legacy) |
| Session | secondary — `solana` feature 한정 (Solana 전용 legacy) |
```

---

## Feature 플래그 재편

### 현재 → 변경 후

| Feature | 현재 | 변경 후 |
|---------|------|---------|
| `evm`   | opt-in (pay-core, pay CLI) | **제거** — EVM은 무조건 컴파일 |
| `solana`| 없음 | **신설** — Solana/MPP/Session opt-in |

### Default

```toml
[features]
default = []   # EVM only (Solana는 명시 활성화)
```

### `solana` feature가 게이트하는 의존성 (pay-core/Cargo.toml)

```toml
solana = [
    "dep:solana-mpp",
    "dep:solana-x402",
    "dep:ed25519-dalek",
    "dep:bs58",
    "dep:bincode",
    "dep:solana-hash",
    "dep:solana-instruction",
    "dep:solana-message",
    "dep:solana-pubkey",
    "dep:solana-signature",
    "dep:solana-system-interface",
    "dep:solana-transaction",
]
```

EVM 의존성(`alloy`, `x402-chain-eip155`, `x402-types`, `hex`, `parking_lot`, `zeroize`)은
기존처럼 무조건 포함 (더 이상 `optional = true` 아님).

---

## 영향 범위

### pay-core (25개 파일)

#### 전체 파일 게이팅 (순수 Solana 모듈)

| 파일 | 처리 방식 |
|------|---------|
| `client/mpp.rs` | `mod.rs`에서 `#[cfg(feature = "solana")]` 선언 |
| `client/session.rs` | 동일 |
| `server/payment.rs` | 동일 (Solana MPP 미들웨어) |
| `server/x402_payment.rs` | 동일 (Solana x402 서버) |
| `server/session.rs` | 동일 |

#### 부분 게이팅 (EVM·Solana 인터리빙)

**`chain.rs`**
- `ChainFamily::Solana` variant → `#[cfg(feature = "solana")]`
- `from_network_slug` / `from_caip2` / `to_network_slug` Solana 분기 → `#[cfg]`
- `is_solana()` → `#[cfg]` 또는 항상 `false` 반환

**`signer.rs`**
- 파일 대부분이 Solana (ed25519 keypair 로딩)
- `load_keypair_bytes_from_account_with_intent()` 전체 → `#[cfg(feature = "solana")]`
- EVM signer 분기만 무조건 유지

**`accounts.rs`**
- `secret_key_b58`, `path` 필드 → `#[cfg(feature = "solana")]`
- `generate_solana_ephemeral_account()` → `#[cfg(feature = "solana")]`
- `is_evm()` / `evm_key_bytes()` 등 EVM 경로는 무조건 유지

**`runner.rs`**
- `RunOutcome::MppChallenge` / `RunOutcome::SessionChallenge` variant → `#[cfg(feature = "solana")]`
- `classify_402()` 내 MPP 파싱 블록 (`mpp::parse_headers`, `is_solana_charge_challenge`) → `#[cfg]`
- Solana x402 감지 헤더 (`solana_x402::PAYMENT_REQUIRED_HEADER`) → `#[cfg]`
- `handle_outcome()` MPP/Session arm → `#[cfg]`

**`client/x402.rs`**
- 파일 상단 `use solana_x402::...` import → `#[cfg(feature = "solana")]`
- `build_payment()` Solana 분기 → `#[cfg]`
- `select_best_chain()` Solana 엔트리 필터링 → `#[cfg]`
- `sign_in_with_x()` → `#[cfg(feature = "solana")]`
- `normalize_network()` / `caip2_for_network()` Solana CAIP-2 분기 → `#[cfg]`
- Solana x402 테스트 블록 → `#[cfg(test)]` + `#[cfg(feature = "solana")]`

**`client/balance.rs`**
- `get_solana_balances()` → `#[cfg(feature = "solana")]`
- `get_balances()` Solana 분기 → `#[cfg]`

**`server/proxy.rs`**
- Solana 프로토콜 디스패치 분기 → `#[cfg(feature = "solana")]`

### pay (CLI, 23개 파일)

#### Cargo.toml

```toml
[features]
default = []
solana = ["pay-core/solana", "dep:solana-mpp", "dep:solana-pubkey", "dep:ed25519-dalek", "dep:bs58"]
gcp_kms = ["solana-mpp/gcp_kms"]   # solana 활성화 시에만 유효
```

CLI 코드에서 `#[cfg(feature = "solana")]`가 필요한 주요 파일:

| 파일 | 게이팅 대상 |
|------|-----------|
| `commands/account/new.rs` | Solana ephemeral 생성 분기 |
| `commands/account/import.rs` | `secret_key_b58` import 분기 |
| `commands/account/list.rs` | Solana pubkey 표시 |
| `commands/account/export.rs` | ed25519 키 export |
| `commands/account/destroy.rs` | Solana keystore 삭제 분기 |
| `commands/send.rs` | Solana send 분기 |
| `commands/topup.rs` | Solana topup 분기 |
| `commands/server/start.rs` | Solana MPP 서버 시작 |
| `commands/skills/` | Solana probe/build 분기 |
| `network.rs` | Solana 네트워크 슬러그 |
| `components/account.rs` | Solana pubkey 포맷 |
| `components/link.rs` | Solana explorer 링크 |

### pay-keystore

- `lib.rs` 상단 주석이 "Solana keypairs"를 언급하나 구현은 OS keychain 기반 generic
- ed25519 `import_keypair_with_intent` / `load_keypair` → `#[cfg(feature = "solana")]`
- secp256k1 (EVM) 경로는 무조건 유지

---

## 구현 순서 (4단계)

### 단계 1 — Cargo.toml 재편 (빌드 그래프 먼저)

1. `pay-core/Cargo.toml`
   - `evm` feature 제거, EVM 의존성을 unconditional로 이동
   - `solana` feature 신설, solana-* 의존성을 `optional = true`로 전환
2. `pay/Cargo.toml` (CLI)
   - `evm` feature 제거
   - `solana` feature 신설 (pay-core/solana re-export)
3. `rust/Cargo.toml` (workspace)
   - solana-* 크레이트를 `optional`로 선언하거나 feature 조건부로 변경

검증: `cargo check` (EVM only) + `cargo check --features solana`

### 단계 2 — 순수 Solana 모듈 게이팅

파일 전체를 mod 선언 수준에서 차단. 각 `mod.rs`에:

```rust
#[cfg(feature = "solana")]
pub mod mpp;

#[cfg(feature = "solana")]
pub mod session;
```

server 쪽:
```rust
#[cfg(feature = "solana")]
mod payment;   // Solana MPP 미들웨어

#[cfg(feature = "solana")]
mod x402_payment;  // Solana x402 서버
```

검증: `cargo check` (EVM only) + `cargo check --features solana`

### 단계 3 — 인터리빙 파일 분리

우선순위 순:

1. `chain.rs` — enum variant 게이팅 (단순, 의존성 없음)
2. `accounts.rs` — Solana 필드 게이팅 + YAML 마이그레이션 graceful skip
3. `signer.rs` — ed25519 로딩 게이팅
4. `client/x402.rs` — Solana x402 client 경로 게이팅
5. `runner.rs` — 다음 3가지 동시 처리:
   - `RunOutcome::MppChallenge` / `SessionChallenge` variant 게이팅
   - `classify_402` 디스패치 순서 재정렬 (**x402 헤더 우선 체크**, MPP/Session는 fallback)
   - Solana-비활성 빌드에서 MPP 헤더 수신 시 x402-first 정책에 따른 에러 메시지 출력
6. `client/balance.rs` — Solana balance 게이팅
7. `server/proxy.rs` — Solana 디스패치 게이팅

각 파일 수정 후 즉시 `cargo check` 양방향 검증.

### 단계 4 — CLI 게이팅

- `commands/` 파일들 순차 수정
- `#[cfg(feature = "solana")]`로 Solana 분기 감싸기
- Solana 없는 EVM-only 빌드에서 CLI 명령이 명확한 에러 반환 확인

---

## 에러 메시지 원칙

Solana 기능을 EVM-only 빌드에서 호출 시:

```
Network 'mainnet' requires Solana support.
Rebuild with --features solana.
```

(Phase 4에서 EVM에 도입한 에러 메시지 패턴 동일 적용)

---

## 설계 원칙 업데이트 (CLAUDE.md)

Phase 20 완료 후 CLAUDE.md의 기존 원칙:

> "`solana` feature는 존재하지 않는다. Solana는 무조건 컴파일된다"

를 다음으로 교체:

> "`evm` feature는 존재하지 않는다. EVM/x402는 무조건 컴파일된다.
> Solana/MPP/Session은 `solana` feature로 opt-in한다."

---

## 테스트 전략

| 빌드 | 테스트 커버 |
|------|-----------|
| `cargo test -p pay-core` | EVM-only 경로 전체 |
| `cargo test -p pay-core --features solana` | Solana + EVM 통합 |
| `cargo test -p pay --features solana` | CLI Solana 명령 |
| `cargo test -p pay-core --features solana,network_tests` | 실 네트워크 (Sepolia 등) |

기존 `evm` feature 테스트(`--features evm`)는 단순히 `--features` 없는 빌드로 대체.

---

## 사전 조사 항목

구현 전 확인 필요:

1. `solana_x402::PAYMENT_REQUIRED_HEADER` 상수가 `#[cfg]` 없이 참조되는 위치 전수 조사
   (특히 `runner.rs`의 classify, x402-first 디스패치 순서 변경에 영향)
2. `runner.rs`의 `RunOutcome` enum — `MppChallenge`/`SessionChallenge` variant 제거 시
   `handle_outcome()` match arm exhaustiveness 컴파일러 경고 확인
3. `accounts.rs` — `secret_key_b58` 없는 EVM-only 계정으로 기존 Solana YAML 로드 시
   graceful skip + WARN 경로 동작 확인 (강제 실패 금지, 하위 호환 보장)
4. `pay-keystore` — `import_keypair_with_intent` 시그니처가 ed25519에 묶여 있는지,
   또는 generic bytes로 추상화 가능한지 확인
5. `classify_402` 디스패치 순서 — 현재 MPP 헤더가 먼저 체크되는데 이를 x402 우선으로
   바꿔도 기존 Solana MPP 통합 테스트가 모두 통과하는지 확인 (x402와 MPP는
   다른 헤더이므로 충돌 없음을 가정하지만 실제 테스트로 입증 필요)
6. CLI commands/server/start.rs — Solana MPP 서버 시작 분기와 EVM x402 서버 시작
   분기가 같은 cfg 게이트로 깔끔히 나뉘는지 확인

---

## 완료 기준

### 빌드/의존성
- [ ] `cargo build` (features 없음) — EVM+x402만 포함, solana-* 의존성 0개
- [ ] `cargo build --features solana` — 기존 동작 전부 유지
- [ ] `cargo tree -p pay-core | grep -c solana` → 0 (features 없는 빌드)

### 테스트
- [ ] `cargo test` — EVM-only 테스트 전부 통과
- [ ] `cargo test --features solana` — Solana 포함 테스트 전부 통과
- [ ] `cargo test --features solana,network_tests` — 실 네트워크 통합 테스트 통과

### x402-first 정책 검증
- [ ] `classify_402` 디스패치 순서가 x402 → MPP/Session으로 재정렬됨
- [ ] EVM-only 빌드에서 `RunOutcome` enum이 `X402Challenge`만 포함하고
      `MppChallenge`/`SessionChallenge` variant는 컴파일되지 않음
- [ ] EVM-only 빌드에서 MPP 헤더 수신 시 "rebuild with --features solana" 메시지 출력
- [ ] Solana 항목이 포함된 기존 `accounts.yml`을 EVM-only 빌드로 로드 시
      graceful skip + WARN 로그 (강제 실패 금지)

### 문서
- [ ] EVM-only 빌드에서 Solana CLI 명령 호출 시 명확한 에러 메시지 출력
- [ ] CLAUDE.md 결제 프로토콜 표 업데이트 (x402 primary, MPP/Session secondary)
- [ ] CLAUDE.md 설계 원칙 업데이트 ("evm feature는 존재하지 않는다, solana는 opt-in")
