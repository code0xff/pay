# Phase 18: 클라이언트 multi-candidate 선택

## 배경

Phase 17 이 서버 측에서 `[chain × currency]` 카르테시안 advertising 을
완성하면서 클라이언트 envelope 의 `accepts[]` 크기가 6+ 까지 자란다.
그러나 현재 클라이언트는:

- `client::x402::select_best_chain` (x402.rs:317) 가 **chain 만** 보고
  첫 entry 를 선택한다. 같은 chain 의 다른 currency 는 거의 항상
  alphabetical 순서로 잡혀서 USDT 가 USDC 보다 먼저 advertised 되면
  USDT 로 결제된다.
- `client::evm::pick_best_candidate` (evm.rs:80) 는 SDK 후보 중 `.next()`
  로 첫 entry 만 사용. "여러 candidate 가 들어오는 경우" 의 명시적
  ranking 없음.
- CLI 에 `--currency` 가 없어서 사용자가 토큰을 강제 선택할 수단이 없다.

결과: 사용자가 USDC 를 들고 있지만 서버가 USDT 를 먼저 advertise 하면
지갑에 잔액이 있는 USDC 가 아닌 USDT 로 시도되다가 잔액 부족으로 실패.

## 목표

`select_best_chain` 를 (chain, currency) 양축에서 선택하도록 확장한다.
우선순위 룰을 도입해 운영 환경에서 안정적이고 예측 가능한 토큰 선택을
보장한다.

비목표:
- 잔액 기반 동적 ranking — RPC 호출이 결제 latency 를 늘리고 RPC 실패에
  취약하므로 별도 phase 로 분리 (Phase 19+).
- multi-account selection — `account_override` (CLI `--account`) 가 이미
  존재. 본 phase 와 직교.

## 원칙

1. **명시 > 암시** — CLI `--currency` 가 모든 default 룰을 override.
2. **기본 선호는 운영 통계 기반** — USDC 가 사실상 표준. USDT 는
   Tether 사용자 백업. 그 외는 alphabetical fallback.
3. **체인 우선순위가 currency 우선순위보다 강하다** — Phase 16/17 의
   chain 라우팅 결정을 currency 가 뒤집지 않는다. 같은 chain 안에서만
   currency 가 ties 를 깬다.
4. **Solana 경로 무변경** — Solana 도 USDC > USDT 룰 같이 적용되지만,
   기존 Solana 단일 currency 운영자는 영향 없음 (accepts 가 1개라 룰
   적용 결과 동일).

---

## 18-1. Currency 선호도

### 신규 헬퍼

```rust
/// Returns a sort key for an accepts entry's currency. Lower is more
/// preferred. Unknown symbols sort last.
fn currency_preference(symbol: &str) -> u8 {
    match symbol.to_ascii_uppercase().as_str() {
        "USDC" => 0,
        "USDT" => 1,
        _ => 99,
    }
}
```

`symbol` 은 `PaymentRequirements.currency` (Solana SPL mint 또는 EVM ERC-20
주소) 가 아니라 **정규화된 심볼**. 매핑은 `skills::probe::normalize_currency`
와 `balance::evm_symbol_for` 를 재사용.

---

## 18-2. `select_best_chain` 확장

### 시그니처 변경

```rust
pub fn select_best_chain<'a>(
    accepts: &'a [PaymentRequirements],
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    currency_override: Option<&str>,  // 신규
) -> Option<&'a PaymentRequirements>;
```

### 우선순위 알고리즘

각 priority 단계에서 "현재 후보 집합" 을 좁히는 방식으로 동작:

1. **`network_override`** — 매칭 entry 만 남김. 비면 None.
2. **계정 매칭 (configured account on slug)** — 남은 후보 중 매칭만 남김.
3. **chain family 선호** (Phase 16: EVM-first under `cfg(feature = "evm")`)
   — 남은 후보 중 매칭 family 만 남김. (매칭 0개면 narrow 하지 않음.)
4. **`currency_override`** — 남은 후보 중 매칭 currency 만 남김.
5. **기본 currency 선호 (USDC > USDT > …)** — `currency_preference` 가
   가장 낮은 값을 가진 첫 entry 반환.

핵심: `Option<&PaymentRequirements>` 가 아니라 중간에 `Vec<&...>` 를
계산. 마지막 단계에서 `min_by_key(currency_preference)`.

### Currency 정규화

EVM entry 는 `currency` 가 `0x…` 주소이므로 `evm_symbol_for(chain_id,
address)` 로 심볼 lookup. Solana entry 는 SPL mint 주소이므로
`normalize_currency` (이미 존재) 로 변환.

`network` 가 `eip155:<id>` 면 chain_id 파싱 → `evm_symbol_for`.
`solana:` 또는 legacy slug 면 `normalize_currency`.

---

## 18-3. `pick_best_candidate` 정렬

`client::evm::pick_best_candidate` 는 SDK 가 emit 한 모든 candidate 중에서
선택한다. SDK 가 이미 점수화한다고 가정하고 첫 항목 채택은 합리적이나,
같은 currency 그룹 안에 여러 candidate 가 있을 때만 SDK 순서 채택. 다른
currency 가 섞여 들어오면 우리 `currency_preference` 로 정렬.

(현재 운영상으로는 SDK 가 candidate 1개만 반환하지만, 향후 SDK 가
multi-candidate 를 지원하면 본 phase 의 정책이 자동으로 적용됨.)

---

## 18-4. CLI `--currency` 추가

`cli/src/commands/curl.rs` (혹은 root-level args) 에 `--currency
<SYMBOL>` 추가. 값은 그대로 `client::x402::build_payment` 의
`currency_override` 로 전달.

### Help text

```
--currency <SYMBOL>      Prefer this stablecoin when the server advertises
                         multiple options (e.g. USDC, USDT). Defaults to
                         USDC when both are advertised.
```

---

## 18-5. `build_payment` 시그니처 확장

```rust
pub fn build_payment(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
    currency_override: Option<&str>,  // 신규
) -> Result<BuiltPayment>;
```

호출부:
- `runner.rs` payment path
- 테스트

기존 호출자에 `None` 을 추가 (단순 forwarding).

---

## 18-6. 테스트

- `select_best_chain_prefers_usdc_over_usdt_by_default` — 같은 chain 에
  USDC/USDT 양쪽 advertise → USDC 선택
- `select_best_chain_currency_override_wins_over_default` — override 가
  USDT 면 USDC 보다 USDT 우선
- `select_best_chain_currency_override_does_not_cross_chains` — override
  가 chain 라우팅을 깨지 않는지 (override 매칭이 chain 우선순위 뒤)
- `select_best_chain_unknown_currency_falls_back_to_first` — 운영자가
  PYUSD 만 advertise 하고 override 가 USDC 일 때 → PYUSD 반환 (fallback)
- `currency_preference_orders_usdc_first` — pure unit test

---

## 변경 파일

| 파일 | 변경 |
|------|------|
| `crates/core/src/client/x402.rs` | `select_best_chain` 시그니처/로직, `build_payment` 시그니처, `currency_preference` 헬퍼 |
| `crates/core/src/client/evm.rs` | `pick_best_candidate` 정렬 (선택적) |
| `crates/core/src/client/runner.rs` | `build_payment` 호출부 `currency_override` 전달 |
| `crates/cli/src/commands/curl.rs` (또는 main args) | `--currency` 플래그 |

---

## 우선순위

P1 — Phase 17 multi-accepts 가 안정적으로 동작하려면 클라이언트 측
currency 결정이 필요. 그렇지 않으면 운영자가 advertise 순서를 신경 써야
하는 비효율 발생.

## Phase 종료 조건

- [ ] `select_best_chain` currency 선호 단위 테스트
- [ ] `--currency` CLI flag 통합
- [ ] 회귀 없음 (이전 phase 단위 테스트 모두 통과)
- [ ] CLAUDE.md 강화 트랙 표에 Phase 18 추가
