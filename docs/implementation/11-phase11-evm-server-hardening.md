# Phase 11: EVM x402 서버 강화 (P0)

## 목표

Phase 6 에서 도입된 EVM x402 게이트웨이는 facilitator 의 `verify`/`settle`
응답을 신뢰하여 통과시키는 **MVP 수준**이다. 본 Phase 는 production 운영 전
반드시 메워야 하는 세 가지 안전성 결손을 닫는다.

| 항목 | 현재 동작 | 위험 |
|-----|--------|------|
| settle 응답을 그대로 신뢰 | facilitator 가 `success:true` 면 통과 | 악성/오작동 facilitator → 무료 트래픽 통과 |
| `(from, nonce)` 재사용 검사 없음 | facilitator 가 같은 envelope 에 idempotent 하게 같은 tx hash 를 반환하면 통과 | 한 결제로 N 회 통과 |
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

### 위협 모델 — 두 단계로 분리

x402 EIP-3009 envelope 의 nonce 는 **USDC 컨트랙트의 `_authorizationStates[from][nonce]`** 에
영구히 기록된다. `transferWithAuthorization` 이 두 번째로 호출되면 contract
가 revert. 그러나 게이트웨이가 *forward 결정* 을 내려야 하는 시점에 그 권위
적 상태가 항상 일치하지 않는다.

```
T0   : envelope E 도착 → contract state = false → 통과 ✓
T0+1 : facilitator.settle → tx broadcast, 아직 mining 안 됨
T0+2 : 같은 envelope E 도착 → state 여전히 false → 또 통과 ✗
...
T+12s: tx mined → state = true (그제서야)
```

mining 윈도우 (Ethereum 12s, Base 2s) 동안은 on-chain 권위만으로도 부족.
facilitator 가 idempotent 하게 같은 tx_hash 를 `success:true` 로 반환하면
Phase 11-1 receipt 검증도 통과해버린다 — 같은 결제로 두 번 forward.

따라서 두 가지 보호를 **모두** 적용한다.

| 시점 | 위협 | 보호 |
|------|------|------|
| mining 후 sequential replay | nonce 가 already-used | `authorizationState(from, nonce)` |
| mining 윈도우 내 parallel replay | gateway 자체 race | in-flight `HashSet<NonceKey>` |

### 변경 설계 — InFlight + authorizationState

**(1) In-flight HashSet** — 처리 중인 `(chain_id, from, nonce)` 만 보유.
용량은 동시 처리 결제 수만큼 (수십~수천), guard 가 Drop 시 자동 해제 →
eviction 우회 공격 불가능.

```rust
pub struct InFlight {
    set: parking_lot::Mutex<HashSet<NonceKey>>,
}
pub struct InFlightGuard<'a> {
    owner: &'a InFlight,
    key: NonceKey,
}
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.owner.set.lock().remove(&self.key);
    }
}
impl InFlight {
    pub fn new() -> Self { Self { set: Mutex::new(HashSet::new()) } }
    pub fn try_acquire(&self, key: NonceKey) -> Option<InFlightGuard<'_>> {
        let mut g = self.set.lock();
        if !g.insert(key) { return None; }
        Some(InFlightGuard { owner: self, key })
    }
}
```

**(2) On-chain `authorizationState`** — EIP-3009 표준 인터페이스. alloy
`sol!` 매크로로 호출. 권위적 ground truth.

```rust
alloy::sol! {
    #[sol(rpc)]
    interface IEip3009 {
        function authorizationState(address authorizer, bytes32 nonce)
            external view returns (bool);
    }
}

async fn check_authorization_state(
    rpc_url: &str,
    asset: alloy::primitives::Address,
    from: alloy::primitives::Address,
    nonce: alloy::primitives::B256,
) -> Result<bool, String> {
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    IEip3009::new(asset, &provider)
        .authorizationState(from, nonce)
        .call()
        .await
        .map_err(|e| format!("authorizationState eth_call failed: {e}"))
}
```

### 통합 지점

`handle_payment` 흐름:

```
1. decode envelope → NonceKey
2. in_flight.try_acquire(key) — guard (drop 시 자동 해제)
     │ None → "payment already being processed" 거절
3. authorizationState(asset, from, nonce)
     │ true → "nonce already used on-chain" 거절
     │ Err → internal_error (fail-closed)
4. facilitator.verify
5. facilitator.settle
6. verify_onchain_receipt   ← Phase 11-1
7. forward (guard Drop)
```

### 왜 LRU 가 아닌가

| 시나리오 | LRU | InFlight + authState |
|---------|-----|---------------------|
| Mining 후 replay | eviction 시 ✗ | ✓ (authState) |
| Mining 윈도우 내 parallel | ✓ (mutex) | ✓ (lock) |
| 용량 한계 우회 공격 | **✗** | ✓ (lock set 은 동시 처리 수만큼) |
| facilitator idempotent (같은 tx_hash 재반환) | LRU 살아있어야 catch | ✓ (authState 가 항상 catch) |
| Cluster 일관성 | 노드별 | authState=자동 / lock=노드별 |
| RPC 비용 | 0 | +1 `eth_call` per request |

LRU 의 본질적 약점 — 50,000 entries 를 attacker 가 50,001번째 noise 로
밀어내면 evict → replay 가능 — 을 in-flight set 은 **동시 처리 수에 비례**
하므로 우회 불가능. 권위적 검사는 on-chain 에 위임.

### 클러스터 동작

In-flight lock 은 노드별:

- 다른 노드가 같은 envelope 처리 시도 → 둘 다 facilitator 호출 →
  facilitator 가 두 번 broadcast → 컨트랙트가 한 개만 mine, 나머지 revert
  → Phase 11-1 의 `receipt.status() == false` 가 reverted 거절.

안전성은 동등 (한 결제 = 한 forward), 단 facilitator 호출 1회 낭비. 클러스터
용 distributed lock (Redis SETNX 등) 은 비용/복잡도 대비 효익이 낮아
별도 트랙으로 미룬다.

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
| `rust/crates/core/src/server/evm_x402_payment.rs` | 수정 | `verify_onchain_receipt`, in-flight guard, `authorizationState` 호출, `PAYMENT-RECEIPT` 헤더, `resolve_amount_usd` Result 화 |
| `rust/crates/core/src/server/in_flight.rs` | **신규** | `InFlight`, `InFlightGuard`, `NonceKey` (envelope 파서) |
| `rust/crates/core/src/server/mod.rs` | 수정 | `pub mod in_flight;` |
| `rust/crates/core/src/lib.rs` | 수정 | `PaymentState::evm_in_flight()` 추가 |
| `rust/crates/cli/src/commands/server/evm_x402_start.rs` | 수정 | `operator.rpc_url` 미설정 시 부팅 거절, `InFlight::new()` 주입 |
| `rust/Cargo.toml` | 수정 | `parking_lot = "0.12"` workspace 의존성 (LRU 불필요) |
| `rust/crates/core/Cargo.toml` | 수정 | `parking_lot` 을 `evm` feature 옵셔널로 추가 |

---

## 테스트 전략

### Unit

- `verify_onchain_receipt_accepts_matching_transfer` — 모의 receipt 에서
  Transfer 이벤트 디코딩 성공
- `verify_onchain_receipt_rejects_underpaid_transfer` — value < expected
  → 명확한 에러
- `verify_onchain_receipt_rejects_wrong_recipient` — to ≠ expected
- `verify_onchain_receipt_rejects_reverted_tx` — receipt.status() = false
- `in_flight_try_acquire_blocks_parallel` — 같은 NonceKey 가 guard 살아있는 동안 두 번째 `try_acquire` 호출 시 None
- `in_flight_guard_releases_on_drop` — guard 해제 후 동일 key 재획득 가능
- `authorization_state_call_decodes_bool` — alloy 인터페이스 응답을 bool 로 받기 (mock provider 또는 통합)
- `resolve_amount_usd_propagates_metering_failure` — 가격 없음 → Err
- `evm_x402_response_carries_payment_receipt_header` — `next.run` 응답에
  `PAYMENT-RECEIPT: 0x...` 부착 확인

### 통합 (`evm,network_tests`)

- Sepolia facilitator 와 실제 결제 한 번 → tx_hash 회수 → 다시 같은 헤더로
  재시도 → mining 후이면 `authorization_state=true` 거절, mining 윈도우
  내라면 `in_flight=busy` 거절.
- `verify_onchain_receipt` 가 reorg 직후 일시적 `tx not yet mined` 를 어떻게
  처리하는지 — 옵션 1: 즉시 verification_failed, 옵션 2: 짧은 retry. Phase 11
  에서는 옵션 1 (단순). retry 는 후속 트랙.

---

## 마이그레이션 가이드

기존 `pay server` 운영자는 다음을 확인해야 한다:

1. `protocol: x402` + EVM 네트워크 spec 에 `operator.rpc_url` 이 있어야 한다.
   없으면 부팅 거절. EVM 잔액 조회용으로 이미 채워둔 경우가 많음.
2. `InFlight` lock 은 노드별이지만, on-chain `authorizationState` 가 권위적
   ground truth 라 cluster 에서도 한 결제 = 한 forward 가 유지된다. 다른
   노드의 parallel race 는 facilitator 호출 1회 낭비로 끝남 (받는 쪽
   컨트랙트가 revert, Phase 11-1 receipt 검증이 거절). Redis SETNX 등 distributed
   lock 은 비용/복잡도 대비 효익이 낮아 follow-up 트랙으로 분리.

---

## 비-목표

- Solana 미들웨어 receipt 재검증 — 별도 트랙
- Redis 기반 distributed in-flight lock — 별도 트랙 (단일 노드에서는 불필요)
- EVM facilitator 자체 운영 — 본 프로젝트 스코프 외
