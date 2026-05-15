# Phase 11: EVM x402 서버 강화 (P0)

## 목표

Phase 6 에서 도입된 EVM x402 게이트웨이는 facilitator 의 `verify`/`settle`
응답을 신뢰하여 통과시키는 **MVP 수준**이다. 본 Phase 는 production 운영 전
반드시 메워야 하는 세 가지 안전성 결손을 닫는다.

| 항목 | 현재 동작 | 위험 |
|-----|--------|------|
| settle 응답을 그대로 신뢰 | facilitator 가 `success:true` 면 통과 | 악성/오작동 facilitator → 무료 트래픽 통과 |
| `(from, nonce)` 재사용 검사 없음 | 동시 두 요청에 같은 결제 헤더 → 둘 다 forward | 한 결제로 N 회 통과 |
| `tx_hash` 응답 헤더 미부착 | 텔레메트리에만 기록 | 클라이언트가 영수증 회수 불가 |
| `resolve_amount_usd` 가 0.01 USD silent fallback | 가격 조회 실패 = 1¢ 결제 | 모니터링 누락 시 무료 결제로 둔갑 |

근거 파일: `rust/crates/core/src/server/evm_x402_payment.rs`
근거 라인: 263–292 (settle 신뢰), 250–271 (재사용 미차단),
280–287 (영수증 미노출), 378–387 (가격 fallback).

---

## 원칙

1. **fail-closed** — 검증 단계 추가는 모두 "통과 못하면 거절", 무거운 외부 호출
   실패도 "결제 통과" 로 미루지 않는다.
2. **Solana 미들웨어 동작 불변** — 본 Phase 의 변경은 모두 EVM 분기에만 적용.
   `server::x402_payment` (Solana) 코드 경로는 한 줄도 건드리지 않는다.
3. **facilitator API 호환성 유지** — 외부 facilitator 의 응답 형태는 그대로 받되,
   추가 검증만 게이트웨이 측에서 수행한다.

---

## 11-1. settle 후 on-chain 영수증 검증

### 현재 동작

`evm_x402_payment.rs:264-292`:

```rust
let settle = facilitator.settle(&payment_payload, &requirements).await?;
if !settle.success { return verification_failed(...); }
let tx_hash = settle.transaction.unwrap_or_default();
// → 그대로 next.run(req).await
```

`settle.transaction` 이 비어 있어도 `unwrap_or_default()` → 빈 문자열로
통과한다. tx 가 있어도 *실제 이체 금액·수신자가 envelope 과 일치하는지*는
확인하지 않는다.

### 변경 설계

`handle_payment` 마지막 단계에 `verify_onchain_receipt` 를 추가:

```rust
async fn verify_onchain_receipt(
    rpc_url: &str,
    tx_hash: &str,
    expected_recipient: &str,
    expected_asset: &str,
    expected_min_amount: u128,
) -> Result<(), String> {
    if tx_hash.is_empty() {
        return Err("settle response missing transaction hash".into());
    }
    let provider = alloy::providers::ProviderBuilder::new()
        .on_http(rpc_url.parse().map_err(|e| format!("{e}"))?);
    let receipt = provider
        .get_transaction_receipt(tx_hash.parse().map_err(|e| format!("{e}"))?)
        .await
        .map_err(|e| format!("eth_getTransactionReceipt failed: {e}"))?
        .ok_or_else(|| "transaction not yet mined".to_string())?;

    if !receipt.status() {
        return Err("transaction reverted on-chain".into());
    }

    // ERC-20 Transfer(address indexed from, address indexed to, uint256 value)
    let transfer_topic = alloy::primitives::keccak256("Transfer(address,address,uint256)");
    let to_topic = address_to_topic(expected_recipient)?;
    let asset_addr = expected_asset.parse::<alloy::primitives::Address>()
        .map_err(|e| format!("invalid expected_asset: {e}"))?;

    let matched = receipt.inner.logs().iter().any(|log| {
        log.address() == asset_addr
            && log.topics().first() == Some(&transfer_topic)
            && log.topics().get(2) == Some(&to_topic)
            && {
                let value = alloy::primitives::U256::from_be_slice(log.data().data.as_ref());
                value >= alloy::primitives::U256::from(expected_min_amount)
            }
    });

    if !matched {
        return Err(format!(
            "on-chain receipt does not contain Transfer({asset_addr} → {expected_recipient}, ≥ {expected_min_amount})"
        ));
    }
    Ok(())
}
```

### RPC URL 출처

게이트웨이는 EVM 키를 보유하지 않으므로 `signer` 기반 RPC 가 없다.
RPC URL 은 `operator.rpc_url` (이미 YAML 에 존재) 에서 가져온다. 미설정 시
부팅 시점에 거절 — `evm_x402_start.rs` 의 가드에 한 줄 추가:

```rust
if operator.rpc_url.is_none() {
    return Err(...);  // facilitator_url 가드와 동일한 패턴
}
```

### 통합 지점

`handle_payment` 흐름:

```
verify → settle → ✦ verify_onchain_receipt ✦ → record_payment_collected → next.run
```

검증 실패 시 `verification_failed_response("on-chain receipt mismatch: ...")`.

### `state.facilitator()` 와 동격으로 `state.evm_rpc_url(network)` 노출

`PaymentState` trait 에 EVM 전용 메서드 한 줄 추가:

```rust
#[cfg(feature = "evm")]
fn evm_rpc_url(&self, network: &str) -> Option<&str>;
```

기본 구현은 `apis` 순회 후 매칭되는 spec 의 `operator.rpc_url` 반환.

---

## 11-2. `(from, nonce)` 재사용 방지

### 위협 모델

x402 EIP-3009 envelope 의 `from` + `nonce` 는 facilitator 단에서 on-chain
재사용 방지가 작동한다 (`transferWithAuthorization` 이 nonce 를 컨트랙트
storage 에 기록). **그러나** 게이트웨이는 verify→settle 두 번의 HTTP 호출
사이에 사용자가 동일 헤더로 다른 요청을 보내면 두 번 다 forwarding 한다.
facilitator 가 두 번째 settle 을 거절하더라도, **첫 번째 forward 는 이미
끝났다**. → 한 결제로 두 API 호출 통과 가능.

### 변경 설계

`AppState` 에 in-memory LRU 추가:

```rust
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;

pub struct NoncesSeen {
    inner: Mutex<LruCache<NonceKey, ()>>,
}
#[derive(Hash, Eq, PartialEq)]
struct NonceKey {
    chain_id: u64,
    from: [u8; 20],
    nonce: [u8; 32],
}
impl NoncesSeen {
    pub fn new(capacity: usize) -> Self { /* NonZeroUsize::new(capacity) */ }
    /// Returns true on first insert, false if already seen.
    pub fn insert(&self, key: NonceKey) -> bool {
        let mut g = self.inner.lock();
        if g.contains(&key) { return false; }
        g.put(key, ());
        true
    }
}
```

### 통합 지점

`handle_payment` 첫 단계 (decode 직후):

```rust
let key = NonceKey::from_payload(&payment_payload).map_err(...)?;
if !state.nonces_seen().insert(key) {
    telemetry::record_settlement_error("x402_evm", subdomain, path, "duplicate_nonce", false);
    return verification_failed_response("payment already used (duplicate nonce)");
}
```

`NonceKey::from_payload` 는 envelope 에서 `payload.authorization.from` /
`payload.authorization.nonce` / `requirements.network` 의 chain_id 를 추출.
형식 오류 시 verification_failed → 즉시 거절.

### 사이즈 / TTL

- 기본 capacity: 50,000 (단일 서버 기준 ~수십분 트래픽)
- TTL 은 LRU 만으로 충분 — EIP-3009 의 `validAfter`/`validBefore` 가 이미
  envelope 단에서 시간 윈도를 좁히기 때문.
- 클러스터링 시 Redis 백엔드는 별도 트랙(Phase 14 후보)으로 분리.

---

## 11-3. `tx_hash` 응답 헤더 노출

### 현재 동작

Solana x402 미들웨어는 `PAYMENT-RECEIPT` 응답 헤더로 트랜잭션 시그니처를
반환 (`server/x402_payment.rs:499-503`). EVM 미들웨어는 tx_hash 를
`tracing::info!` 와 telemetry 에만 기록한다.

### 변경 설계

`handle_payment` 의 `next.run(req).await` 결과에 응답 헤더 부착:

```rust
let mut response = next.run(req).await;
if let Ok(value) = axum::http::HeaderValue::from_str(&tx_hash) {
    response.headers_mut().insert(
        solana_x402::PAYMENT_RECEIPT_HEADER,  // 이미 Solana 와 동일 상수 사용
        value,
    );
}
return response;
```

클라이언트는 `runner.rs::collect_x402_receipt` 가 이미 헤더를 읽어 stdout
출력에 포함하고 있으므로 추가 client 변경 불필요.

---

## 11-4. 가격 fallback 제거

### 현재 동작

`evm_x402_payment.rs:378-387`:

```rust
fn resolve_amount_usd(meter, props, variant_hint) -> f64 {
    metering::resolve_price(meter, props, variant_hint, None)
        .and_then(|p| p.dimensions.first().cloned())
        .map(|d| d.price_usd / d.scale.max(1) as f64)
        .unwrap_or(0.01)  // ← silent fallback
}
```

가격 조회 실패가 1¢ 결제로 둔갑한다. operator 의 모니터링이 약하면 모든
결제가 1¢ 로 청구되는 버그가 눈치채지 못한 채 유지된다.

### 변경 설계

`Result<f64, String>` 으로 변경하고 실패 시 500:

```rust
fn resolve_amount_usd(...) -> Result<f64, String> {
    let price = metering::resolve_price(meter, props, variant_hint, None)
        .ok_or_else(|| "metering returned no price".to_string())?;
    let dim = price.dimensions.first()
        .ok_or_else(|| "metering price has no dimensions".to_string())?;
    let scale = dim.scale.max(1) as f64;
    Ok(dim.price_usd / scale)
}
```

호출처:

```rust
let amount_usd = match resolve_amount_usd(meter, &props, variant_hint.as_deref()) {
    Ok(v) => v,
    Err(e) => return internal_error(&format!("price_resolution_failed: {e}")),
};
```

> Solana 미들웨어도 동일한 fallback 패턴을 사용 중이라면 함께 수정해야 하지만,
> 본 Phase 의 스코프는 EVM 분기로 한정한다. Solana 측은 `verifier` 가 확인 후
> 별도 follow-up 으로 처리.

---

## 변경 파일 요약

| 파일 | 유형 | 변경 |
|------|------|------|
| `rust/crates/core/src/server/evm_x402_payment.rs` | 수정 | `verify_onchain_receipt`, NonceKey 통합, `PAYMENT-RECEIPT` 헤더, `resolve_amount_usd` Result 화 |
| `rust/crates/core/src/server/nonces.rs` | **신규** | `NoncesSeen` (LruCache 기반) |
| `rust/crates/core/src/server/mod.rs` | 수정 | `pub mod nonces;` |
| `rust/crates/core/src/lib.rs` | 수정 | `PaymentState::evm_rpc_url`, `PaymentState::nonces_seen` 추가 |
| `rust/crates/cli/src/commands/server/evm_x402_start.rs` | 수정 | `rpc_url` 미설정 시 부팅 거절, `NoncesSeen::new(50_000)` 주입 |
| `rust/Cargo.toml` | 수정 | `lru = "0.12"`, `parking_lot = "0.12"` workspace 의존성 |
| `rust/crates/core/Cargo.toml` | 수정 | 위 두 의존성을 `evm` feature 옵셔널로 추가 |

---

## 테스트 전략

### Unit

- `verify_onchain_receipt_accepts_matching_transfer` — 모의 receipt 에서
  Transfer 이벤트 디코딩 성공
- `verify_onchain_receipt_rejects_underpaid_transfer` — value < expected
  → 명확한 에러
- `verify_onchain_receipt_rejects_wrong_recipient` — to ≠ expected
- `verify_onchain_receipt_rejects_reverted_tx` — receipt.status() = false
- `nonces_seen_blocks_duplicate` — 같은 NonceKey 두 번째 insert → false
- `resolve_amount_usd_propagates_metering_failure` — 가격 없음 → Err
- `evm_x402_response_carries_payment_receipt_header` — `next.run` 응답에
  `PAYMENT-RECEIPT: 0x...` 부착 확인

### 통합 (`evm,network_tests`)

- Sepolia facilitator 와 실제 결제 한 번 → tx_hash 회수 → 다시 같은 헤더로
  재시도 → `duplicate_nonce` 거절 확인.
- `verify_onchain_receipt` 가 reorg 직후 일시적 `tx not yet mined` 를 어떻게
  처리하는지 — 옵션 1: 즉시 verification_failed, 옵션 2: 짧은 retry. Phase 11
  에서는 옵션 1 (단순). retry 는 후속 트랙.

---

## 마이그레이션 가이드

기존 `pay server` 운영자는 다음을 확인해야 한다:

1. `protocol: x402` + EVM 네트워크 spec 에 `operator.rpc_url` 이 있어야 한다.
   없으면 부팅 거절. EVM 잔액 조회용으로 이미 채워둔 경우가 많음.
2. 클러스터 배포 시 `NoncesSeen` 이 단일 노드 LRU 이므로, 로드 밸런서 sticky
   session 이 없다면 같은 nonce 가 다른 노드에서 통과될 수 있다. 본 Phase 는
   "best effort" 임을 운영 문서에 명시.

---

## 비-목표

- Solana 미들웨어 receipt 재검증 — 별도 트랙
- Redis 기반 분산 nonce 캐시 — 별도 트랙
- EVM facilitator 자체 운영 — 본 프로젝트 스코프 외
