# Phase 13: EVM x402 프로토콜 정합성 (P1+P2)

## 목표

Phase 11/12 가 안전성·UX 결손을 닫는다면, 본 Phase 는 **프로토콜 정합성** —
서버 envelope 이 facilitator 의 검증 룰과 어긋날 가능성을 제거한다.

| 항목 | 현재 동작 | 위험 |
|-----|--------|------|
| EIP-712 도메인 정적 매핑 | `(chain, "USD Coin"/"USDC", "2")` 하드코딩 | 신규 체인/토큰 deploy 시 침묵 실패 |
| 서버 envelope 수기 JSON | `serde_json::json!` 매크로로 직조 | `x402-chain-eip155` 메이저 업그레이드 시 드리프트 |
| `decimals = 6` 하드코딩 | USDC 가정 | DAI(18) 등 추가 시 침묵 버그 |
| `accepts: [one]` | 단일 후보만 발행 | 멀티 토큰/멀티 체인 advertising 불가 |
| v1 EVM envelope | 서버는 v2 만 발행, v1 헤더는 받음 | 옛 클라이언트와 무한 401/402 루프 |
| `client/evm.rs:90 .next()` | 첫 후보 자동 선택 | 멀티 후보 시 잘못된 토큰 서명 가능 |

근거 파일:
- `rust/crates/core/src/server/evm_x402_payment.rs:328, 344-376, 187-190`
- `rust/crates/core/src/client/evm.rs:88-97`
- `rust/crates/core/src/client/x402.rs:207-220` (v1 분기)

---

## 원칙

1. **단일 진실의 출처는 SDK** — typed builder/parser 로 envelope 을 만들고 읽는다.
   직접 JSON 조립은 SDK 미제공 케이스로 한정한다.
2. **체인/토큰 메타데이터는 캐시된 1회 조회** — 첫 envelope 빌드 시 RPC 로
   `eip712Domain()` / `decimals()` 를 조회하고 `Once` 로 캐시.
3. **버전 호환성은 명시적** — v1 envelope 미지원이라면 명시적 에러를 반환하고,
   서버는 v1 클라이언트가 보낸 헤더를 거절한다 (silent passthrough 금지).

---

## 13-1. EIP-712 도메인 on-chain 조회

### 현재 동작

`evm_x402_payment.rs:368-376` `usdc_eip712_domain` 이 슬러그별 `(name, version)`
을 정적으로 반환. Base 의 native USDC (`0x8335...`) 와 bridge USDbC 는 도메인이
다른데, 게이트웨이가 이를 모르고 잘못된 hint 를 envelope `extra` 에 박는다.
facilitator 가 서명 검증 시 도메인 불일치로 거절 → 사용자는 "invalid signature"
만 봄.

### 변경 설계

**Step 1.** alloy `sol!` 매크로로 `IEIP712Domain` 정의:

```rust
alloy::sol! {
    #[sol(rpc)]
    interface IEip712Domain {
        function eip712Domain() external view returns (
            bytes1 fields,
            string memory name,
            string memory version,
            uint256 chainId,
            address verifyingContract,
            bytes32 salt,
            uint256[] memory extensions
        );
    }
}
```

**Step 2.** `rust/crates/core/src/client/evm_domain.rs` 신규:

```rust
use std::sync::OnceLock;
use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct Eip712TokenDomain {
    pub name: String,
    pub version: String,
}

static CACHE: OnceLock<RwLock<HashMap<(u64, alloy::primitives::Address), Eip712TokenDomain>>> = OnceLock::new();

pub async fn fetch_token_domain(
    rpc_url: &str,
    chain_id: u64,
    token: alloy::primitives::Address,
) -> Result<Eip712TokenDomain, String> {
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Some(d) = cache.read().await.get(&(chain_id, token)) {
        return Ok(d.clone());
    }
    let provider = alloy::providers::ProviderBuilder::new()
        .on_http(rpc_url.parse().map_err(|e| format!("{e}"))?);
    let domain = IEip712Domain::new(token, &provider)
        .eip712Domain()
        .call()
        .await
        .map_err(|e| format!("eip712Domain() reverted: {e}. Token may not implement EIP-5267."))?;
    let value = Eip712TokenDomain { name: domain.name.clone(), version: domain.version.clone() };
    cache.write().await.insert((chain_id, token), value.clone());
    Ok(value)
}

/// Synchronous fallback for tests / cold start. Returns the previously
/// hardcoded values, but logs a warning telling the caller to wire RPC.
pub fn static_fallback_domain(chain_id: u64) -> Eip712TokenDomain {
    let (name, version) = match chain_id {
        1 | 8453 | 10 | 42161 => ("USD Coin", "2"),
        _ => ("USDC", "2"),
    };
    tracing::warn!(chain_id, "Falling back to hardcoded EIP-712 domain — RPC unavailable");
    Eip712TokenDomain { name: name.into(), version: version.into() }
}
```

**Step 3.** `build_evm_requirements` 시그니처 비동기화 + RPC 인자 추가:

```rust
async fn build_evm_requirements(
    rpc_url: &str,
    network_slug: &str,
    recipient: &str,
    currency_symbol: &str,
    amount_usd: f64,
    uri: &Uri,
    description: Option<&str>,
) -> Result<serde_json::Value, String> {
    let chain_id = ...;
    let asset = ...;
    let token: Address = asset.parse().unwrap();
    let domain = match fetch_token_domain(rpc_url, chain_id, token).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "eip712Domain() lookup failed, using fallback");
            static_fallback_domain(chain_id)
        }
    };
    // ...
    Ok(json!({ ..., "extra": { "name": domain.name, "version": domain.version } }))
}
```

**Step 4.** 호출처 (`evm_x402_payment_middleware`) 가 이미 async 이므로 await
체인 한 줄만 추가. 캐시 첫 hit 후에는 RPC 없이 즉시 반환.

### 실패 모드

- RPC 미설정 → `static_fallback_domain` (현재와 동일 동작) + 경고 로그.
- 토큰이 EIP-5267 (`eip712Domain()`) 미구현 → fallback. Phase 11 이 receipt
  검증으로 잘못된 도메인 결제를 catch 하므로 부담은 작다.

---

## 13-2. 서버 envelope typed builder

### 현재 동작

`evm_x402_payment.rs:344-362` 가 `json!` 매크로로 envelope 직접 조립:

```rust
Ok(json!({
    "scheme": "exact",
    "network": format!("eip155:{chain_id}"),
    ...
    "maxAmountRequired": raw_amount.to_string(),
    "amount": raw_amount.to_string(),   // 동일 값
    ...
}))
```

x402 spec 상 `maxAmountRequired` 와 `amount` 의 의미는 미세하게 다르고,
`x402-chain-eip155` 가 메이저 업그레이드되면 필드명/구조가 바뀐다.

### 변경 설계

`x402-chain-eip155` 가 server-side `PaymentRequirements` 직렬화 helper 를
제공하는지 확인. 있으면 직접 사용, 없으면 typed Rust struct 정의 후 serde:

```rust
// rust/crates/core/src/server/x402_envelope.rs
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmPaymentRequirements {
    pub scheme: &'static str,          // "exact"
    pub network: String,                // "eip155:8453"
    pub asset: String,                  // 0x...
    pub pay_to: String,
    pub max_amount_required: String,    // wei 단위 문자열
    pub currency: String,               // 동일 0x...
    pub decimals: u32,
    pub resource: String,
    pub description: String,
    pub max_timeout_seconds: u32,
    pub extra: EvmExtra,
}

#[derive(serde::Serialize)]
pub struct EvmExtra {
    pub name: String,
    pub version: String,
}

#[derive(serde::Serialize)]
pub struct PaymentRequiredEnvelope<'a> {
    #[serde(rename = "x402Version")]
    pub x402_version: u8,
    pub accepts: Vec<&'a EvmPaymentRequirements>,
    pub resource: Option<String>,
}
```

`build_evm_requirements` 가 위 struct 를 반환. 미들웨어가
`serde_json::to_value(&envelope)` 로 헤더 base64 인코딩.

> `amount` 필드는 일부 facilitator 가 요구하므로 `max_amount_required` 와 함께
> 추가하되, 두 값을 다르게 두는 사용 사례가 명확해질 때까지 같은 값으로 둔다.
> 차이가 생길 가능성을 감안해 **두 필드를 독립 struct 필드로 분리**해 둔다.

### 호환성 테스트

`tests/x402_envelope_compat.rs` 에 fixture-based 회귀 테스트:

```rust
#[test]
fn envelope_matches_coinbase_x402_reference_fixture() {
    let env = build_envelope_with_fixed_inputs();
    let actual = serde_json::to_value(&env).unwrap();
    let expected: serde_json::Value = serde_json::from_str(include_str!("fixtures/x402_envelope_base.json")).unwrap();
    assert_eq!(actual, expected);
}
```

fixture 는 Coinbase 의 x402-go/x402-ts SDK 출력에서 캡처.

---

## 13-3. Stablecoin decimals 일반화

Phase 12-5 와 동일한 변경 — 두 Phase 중 어느 쪽에서 머지하든 한 곳만 수정:

```rust
pub fn evm_stablecoin_decimals(symbol: &str) -> u32 {
    match symbol {
        "USDC" | "USDT" => 6,
        "DAI" => 18,
        _ => 6,
    }
}
```

호출처:
- `evm_x402_payment.rs::build_evm_requirements` — `let decimals = evm_stablecoin_decimals(currency_symbol);`
- `client/evm.rs::build_evm_payment` — 동일
- `client/balance.rs::get_evm_balances` — `format_ui_amount(raw, evm_stablecoin_decimals(sym))`

---

## 13-4. 다중 accepts 지원

### 현재 동작

`evm_x402_payment.rs:187-190` `"accepts": [requirements]` — 항상 단일 요구사항만
advertise. 운영자가 USDC + USDT 동시 수령을 원해도 불가능.

### 변경 설계

`OperatorConfig.currencies` 가 이미 `{ "usd": ["USDC", "USDT"] }` 형태로 다중
심볼을 지원한다. 빌더가 모든 심볼에 대해 요구사항을 생성하고 `accepts` 에
push:

```rust
let symbols = pick_currency_symbols(operator);  // pick_currency_symbol → 복수형
let mut accepts = Vec::with_capacity(symbols.len());
for sym in &symbols {
    match build_evm_requirements(..., sym, ...).await {
        Ok(req) => accepts.push(req),
        Err(e) => tracing::warn!(symbol = %sym, error = %e, "Skipping symbol in accepts"),
    }
}
if accepts.is_empty() { return internal_error("No valid currencies for this network"); }
```

클라이언트 측은 `select_best_chain` 이 이미 `accepts` 배열 전체를 보고
account 매칭을 수행하므로 변경 불필요.

### 비고

x402 SDK 가 multi-accept 를 정식 지원하는지 확인이 선행되어야 한다. 일부
구버전은 `accepts[0]` 만 사용. SDK fallback 동작도 unit test 로 고정.

---

## 13-5. v1 EVM envelope 명시적 처리

### 현재 동작

`client/x402.rs:207-220`:
```rust
match challenge.x402_version {
    X402_VERSION_V1 => { build_payment_header_v1(...).await? }
    _              => { build_payment_header_v2(...).await? }
}
```

`build_payment_header_v1` 는 Solana SDK 만 호출. EVM v1 envelope 가 오면 Solana
경로로 들어가 의미 없는 에러 발생.

### 변경 설계

`build_payment` 의 chain dispatch 이전 단계에서 v1 + EVM 조합을 명시 거절:

```rust
if challenge.x402_version == X402_VERSION_V1 {
    if let Some(net) = challenge.requirements.network.strip_prefix("eip155:") {
        return Err(Error::Mpp(format!(
            "x402 v1 is not supported on EVM (network `eip155:{net}`). \
             Upgrade the server to advertise x402 v2."
        )));
    }
}
```

서버 측 (`evm_x402_payment_middleware`) 도 동일: v1 헤더를 받으면 v1 envelope 으로
challenge 를 다시 발급하지 못하므로, v1 헤더는 거절:

```rust
let payment_header = headers
    .get(PAYMENT_SIGNATURE_HEADER)
    .and_then(|v| v.to_str().ok())
    .map(str::to_string);
if headers.contains_key(X402_V1_PAYMENT_HEADER) {
    return verification_failed_response("x402 v1 is not supported on EVM gateways");
}
```

---

## 13-6. Client `accept()` 후보 선택 강화

### 현재 동작

`client/evm.rs:88-97`:
```rust
let candidates = V2Eip155ExactClient::new(signer.local_signer().clone())
    .accept(&payment_required)
    .into_iter()
    .next()                       // ← 첫 후보 무조건
    .ok_or_else(|| Error::Mpp("No EVM payment candidate accepted".into()))?;
```

서버가 `accepts: [USDC, USDT, DAI]` 를 advertise 하면 SDK 가 가장 적합한 후보를
스코어링했더라도 우리는 첫 결과만 사용한다. 토큰 잔액 / 사용자 선호와 무관.

### 변경 설계

```rust
fn pick_best_candidate(
    candidates: Vec<AcceptedRequirement>,
    preferred_symbol: Option<&str>,
) -> Result<AcceptedRequirement, Error> {
    if candidates.is_empty() {
        return Err(Error::Mpp("No EVM payment candidate accepted".into()));
    }
    if let Some(sym) = preferred_symbol {
        if let Some(c) = candidates.iter().find(|c| matches_symbol(c, sym)) {
            return Ok(c.clone());
        }
    }
    // 기본: 보유 잔액이 충족되는 첫 후보, 없으면 첫 후보.
    Ok(candidates.into_iter().next().unwrap())
}
```

`preferred_symbol` 은 `--currency` 플래그가 있다면 그 값, 없으면 `Config` 의
`evm_preferred_stablecoin` (옵셔널).

잔액 기반 자동 선택은 RPC 호출이 필요해 비용이 크다 — 본 Phase 는 "선호 심볼
매칭 후 첫 후보 fallback" 까지만. 잔액 기반은 후속 트랙.

---

## 변경 파일 요약

| 파일 | 유형 | 변경 |
|------|------|------|
| `rust/crates/core/src/client/evm_domain.rs` | **신규** | `fetch_token_domain` (OnceLock 캐시) |
| `rust/crates/core/src/server/evm_x402_payment.rs` | 수정 | `build_evm_requirements` async + 도메인 RPC 조회 + multi-accepts + v1 거절 |
| `rust/crates/core/src/server/x402_envelope.rs` | **신규** | typed `EvmPaymentRequirements` + `PaymentRequiredEnvelope` |
| `rust/crates/core/src/client/x402.rs` | 수정 | v1 + EVM 조합 명시 거절 |
| `rust/crates/core/src/client/evm.rs` | 수정 | `pick_best_candidate`, 심볼 선호 |
| `rust/crates/core/src/client/balance.rs` | 수정 | `evm_stablecoin_decimals` export, 사용 |
| `rust/crates/types/src/stablecoin.rs` | 수정 | `evm_stablecoin_decimals` 정의 |
| `rust/crates/core/src/config.rs` | 수정 | `evm_preferred_stablecoin: Option<String>` |
| `rust/tests/fixtures/x402_envelope_base.json` | **신규** | Coinbase SDK 출력 fixture |

---

## 테스트 전략

### Unit

- `evm_domain_cache_uses_rpc_once` — 두 번 호출 시 첫 번째만 RPC, 두 번째는 캐시
- `static_fallback_domain_emits_warning_log`
- `envelope_typed_serializes_to_camelcase` — `payTo`, `maxAmountRequired`,
  `x402Version` 케이스
- `envelope_typed_with_multi_accepts` — 2 개 심볼 → 길이 2
- `client_evm_picks_preferred_symbol` — `--currency USDT` 시 USDT 후보 선택
- `client_evm_falls_back_to_first_candidate_when_preference_missing`
- `build_payment_rejects_v1_evm_combo`
- `server_rejects_v1_payment_header_on_evm_gateway`

### Fixture 회귀

- `envelope_matches_coinbase_x402_reference_fixture` — Coinbase SDK 출력과
  byte-equivalent (필드 순서·string 인코딩 포함)

### 통합 (`evm,network_tests`)

- Base 메인넷 native USDC 컨트랙트로 `eip712Domain()` 1회 호출 후 envelope 빌드.
  도메인이 `("USD Coin","2")` 와 일치하는지 (현재 fallback 과 일치) 확인 후,
  fallback 비활성화 모드로 재실행해도 동일 envelope 생성.

---

## 비-목표

- ERC-2612 (`permit`) 기반 결제 — x402 spec 외, 별도 트랙
- EIP-3009 `transferWithAuthorizationWithDeadline` 등 변형 스펙 — 표준 확장 시 검토
- v1 envelope 후방 호환 — 명시 거절로 종결
