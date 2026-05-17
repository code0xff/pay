# Phase 16: EVM-first payment routing

## 배경

`pay` 의 운영 방향이 EVM 중심으로 이동했지만 결제 라우팅 코드는 여전히
Solana-first 다.

- `client::x402::parse()` (x402.rs:67) 가
  `parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))`
  로 Solana mainnet entry 를 먼저 찾고, 없으면 `all_accepts.first()` 로
  떨어진다. 그래서 다중 체인 envelope 의 `challenge.requirements` 는
  EVM accepts 가 있어도 Solana entry 로 채워진다.
- `client::x402::select_best_chain` (x402.rs:312) 의 priority 4 는
  "first non-EVM entry" — 명시적으로 Solana 선호. priority 3 는
  "EVM-only 사용자" 한정 EVM 선호 (review 단계에서 추가됨).
- Phase 15 의 `skills::probe::pick_indexable_x402` 는 EVM-first 로
  바뀌었지만 결제 경로는 그대로다 → 인덱스에는 EVM 우선으로 노출되는데
  실제 결제는 Solana 로 가는 비대칭.

## 목표

`cfg(feature = "evm")` 빌드에서 결제 라우팅 전체를 **EVM-first** 로
통일한다. Solana-only 빌드는 영향 없음.

비목표:
- Server-side multi-accepts 발행 (Phase 17)
- Multi-candidate ranking (Phase 18)
- 신규 체인/토큰 추가 (Phase 19)

## 원칙

1. **빌드 feature 가 의도다** — 사용자가 `--features evm` 로 빌드했다면
   EVM 결제가 1순위. Solana 는 fallback 으로만 동작.
2. **`--network` override 가 최우선** — EVM-first 는 default 일 뿐
   사용자 명시 선택을 덮지 않는다.
3. **Solana-only 빌드는 무변경** — `#[cfg(feature = "evm")]` 로 gating
   해 두면 자연스럽게 Solana-first 가 유지된다.
4. **probe 와 paymen 의 우선순위 동기화** — Phase 15 probe 와
   동일한 dispatch 규칙을 결제 경로에도 적용.

---

## 16-1. `client::x402::parse()` EVM-first hint

### 현재

```rust
let requirements = parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))
    .or_else(|| all_accepts.first().cloned())?;
```

### 변경

```rust
let requirements = pick_default_requirement(&all_accepts, headers, body)?;
```

`pick_default_requirement`:
1. `cfg(feature = "evm")` 빌드면 `all_accepts` 중 `eip155:` 접두 entry
   첫 번째를 반환 (없으면 다음 단계)
2. `parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))`
3. `all_accepts.first().cloned()`

이렇게 하면 `Challenge::requirements` (legacy single-entry 소비자) 도
EVM-first 표시.

---

## 16-2. `client::x402::select_best_chain` EVM-first

### 현재 priority (Phase 11-14 review 단계 결과)

1. `network_override`
2. 정규화된 슬러그에 계정이 *있는* 첫 entry
3. 사용자가 EVM-only 일 때 EVM entry
4. **첫 non-EVM entry (Solana 선호)**
5. 첫 entry

### 변경 priority

1. `network_override`
2. 정규화된 슬러그에 계정이 *있는* 첫 entry (변경 없음)
3. **`cfg(feature = "evm")` 빌드 → 첫 EVM entry**
4. 첫 Solana entry
5. 첫 entry

priority 3 가 "EVM-only 사용자" 조건을 떼고 항상 EVM-first 로 동작.
Solana 계정만 있는 사용자도 priority 2 에서 잡히므로 회귀는 없다.

---

## 16-3. 테스트

- `parse_prefers_evm_entry_when_evm_feature_on` — 멀티체인 envelope 의
  `challenge.requirements` 가 EVM entry 인지 확인
- `select_best_chain_prefers_evm_under_evm_feature` — Solana+EVM 둘 다
  계정이 없을 때 EVM entry 선택
- `select_best_chain_solana_only_user_still_works` — Solana 계정만
  설정돼 있으면 priority 2 에서 Solana entry 잡힘 (회귀 방지)
- `select_best_chain_override_wins_over_evm_first` — `--network solana`
  override 시 EVM-first 무시
- `select_best_chain_solana_only_build_unchanged` — `cfg(not(feature =
  "evm"))` 빌드는 기존 동작

---

## 변경 파일

| 파일 | 변경 |
|------|------|
| `crates/core/src/client/x402.rs` | `parse()` EVM-first hint, `select_best_chain` priority 3 변경 |
| `crates/core/src/client/x402.rs` (tests) | 신규 테스트 5건 |

`runner.rs` / `evm.rs` / 서버 미들웨어는 무변경.

---

## 우선순위

P0 — 운영 방향이 EVM 중심으로 이동했으므로 라우팅이 따라가야 한다.
다른 phase 와 작업 격리되는 단일 함수 변경이라 회귀 위험도 낮다.

## Phase 종료 조건

- [ ] `parse()` EVM-first 단위 테스트 통과
- [ ] `select_best_chain` priority 재배열 단위 테스트 통과
- [ ] Solana-only 빌드 회귀 없음
- [ ] CLAUDE.md 강화 트랙 표에 Phase 16 추가
