# Phase 6: x402 Server Payment Proxy

## 개요

`pay server`에 x402 프로토콜 기반 결제 게이트웨이 프록시를 추가한다. 현재 MPP(Solana)만 지원하는 서버 미들웨어를 x402로도 동작하도록 확장한다.

---

## 배경

### 현재 상태

- `pay server start`는 MPP 프로토콜만 지원 (`server/payment.rs`)
- `solana-x402` SDK에 서버 모듈(`solana_x402::server::X402`)이 이미 존재하나, `Cargo.toml`에서 `"client"` feature만 활성화된 상태
- `PaymentProtocol::X402` enum이 `metering.rs`에 이미 정의되어 있으나 `Service` 구조체에서만 사용 중

### 목표

YAML 스펙에 `protocol: x402` 한 줄만 추가하면 x402 기반 402 게이트웨이로 동작하도록 한다. 기존 MPP 스펙 파일은 변경 없이 동작해야 한다.

---

## 와이어 포맷 비교

| | MPP | x402 |
|--|-----|------|
| 챌린지 헤더 | `WWW-Authenticate` | `x-payment-required` |
| 결제 헤더 | `Authorization` | `x-payment` / `x-payment-v1` |
| 챌린지 형식 | Base64 JSON (MPP spec) | Base64 JSON (`PaymentRequiredEnvelope`) |

헤더가 완전히 분리되어 있어 동일 서버에서 두 프로토콜 충돌 없이 공존 가능.

---

## 핵심 SDK API

`solana_x402::server::X402` (feature gate: `"server"`):

```rust
// 생성
X402::new(Config { recipient, currency, decimals, network, rpc_url, .. }) -> Result<X402>

// 챌린지 헤더 생성
X402::payment_required_header(amount, options) -> Result<(header_name, base64_value)>

// 결제 서명 검증
X402::verify_payment_signature(header_value) -> Result<VerifiedExactPayment>

// 검증 결과
enum VerifiedExactPayment {
    Transaction(VersionedTransaction),  // 검증됐으나 미broadcast → 미들웨어에서 직접 전송 필요
    Signature(String),                  // 이미 on-chain 확인됨 → 그대로 포워딩
}
```

---

## 변경 파일

| 파일 | 유형 | 내용 |
|------|------|------|
| `rust/Cargo.toml` | 수정 | `solana-x402` features에 `"server"` 추가 |
| `rust/crates/types/src/metering.rs` | 수정 | `OperatorConfig`에 `protocol: PaymentProtocol` 추가 |
| `rust/crates/core/src/lib.rs` | 수정 | `PaymentState` trait에 `fn x402s()` 추가 |
| `rust/crates/core/src/server/x402_payment.rs` | **신규** | x402 Axum 미들웨어 |
| `rust/crates/core/src/server/mod.rs` | 수정 | `x402_payment` 모듈 export |
| `rust/crates/cli/src/commands/server/start.rs` | 수정 | `AppState`에 `x402s` 추가, 미들웨어 분기 |

---

## 구현 상세

### Step 1 — Feature 활성화

`rust/Cargo.toml`:
```toml
solana-x402 = { ..., features = ["client", "server"] }
```

### Step 2 — `OperatorConfig`에 protocol 추가

`rust/crates/types/src/metering.rs`:
```rust
impl Default for PaymentProtocol {
    fn default() -> Self { PaymentProtocol::Mpp }
}

pub struct OperatorConfig {
    // 기존 필드 유지...
    #[serde(default)]
    pub protocol: PaymentProtocol,
}
```

`Default`를 `Mpp`로 설정하여 기존 YAML 스펙 하위 호환 유지.

### Step 3 — `PaymentState` trait 확장

`rust/crates/core/src/lib.rs`:
```rust
pub trait PaymentState: Clone + Send + Sync + 'static {
    // 기존 메서드 유지...
    fn x402s(&self) -> Vec<&solana_x402::server::X402> { vec![] }
}
```

### Step 4 — x402 미들웨어 신규 작성

`rust/crates/core/src/server/x402_payment.rs`:

```
x402_payment_middleware<S: PaymentState>()
  │
  ├── path == "__402/*" → pass-through
  ├── subdomain으로 ApiSpec 조회 (없으면 pass-through)
  ├── method+path로 metered endpoint 조회 (없으면 pass-through)
  │
  ├── [x-payment 헤더 없음]
  │     └── price 계산 → X402::payment_required_header() → 402 반환
  │
  └── [x-payment 헤더 있음]
        ├── X402::verify_payment_signature() 호출
        ├── Ok(Transaction(tx)) → rpc.send_transaction(&tx) → 성공 시 next.run(req)
        ├── Ok(Signature(_))    → next.run(req) (이미 on-chain 확인)
        └── Err(_)              → 402 + 재챌린지
```

`payment.rs`와 동일한 패턴 적용:
- `metering::find_endpoint()` / `find_endpoint_by_path()` 재사용
- `metering::resolve_price()` 재사용
- `telemetry::record_*()` 재사용

### Step 5 — `AppState` 확장 및 미들웨어 분기

`rust/crates/cli/src/commands/server/start.rs`:

```rust
struct AppState {
    mpps: Vec<Mpp>,
    x402s: Vec<solana_x402::server::X402>,  // 신규
    // ...
}
```

X402 인스턴스 생성 (MPP 생성 패턴 동일, line ~383):
```rust
if operator_config.protocol == PaymentProtocol::X402 {
    let x402 = solana_x402::server::X402::new(solana_x402::server::Config {
        recipient: operator_config.recipient.clone(),
        currency: operator_config.currency.clone(),
        network: operator_config.network.clone(),
        rpc_url: Some(rpc_url.clone()),
        ..Default::default()
    })?;
    app_state.x402s.push(x402);
}
```

미들웨어 분기 (router wiring, line ~875):
```rust
let router = match spec.operator.protocol {
    PaymentProtocol::Mpp  => router.layer(middleware::from_fn_with_state(state.clone(), payment_middleware::<AppState>)),
    PaymentProtocol::X402 => router.layer(middleware::from_fn_with_state(state.clone(), x402_payment_middleware::<AppState>)),
};
```

---

## YAML 스펙 변경

```yaml
operator:
  recipient: "7xKX...abc"
  currency: USDC
  network: mainnet
  protocol: x402        # 신규 (생략 시 기본값: mpp)

endpoints:
  - path: /v1/generate
    method: POST
    metering:
      price_usd: 0.01
```

---

## 주의사항

### Transaction broadcast
`verify_payment_signature()`가 `Transaction(tx)`를 반환하는 경우, 미들웨어에서 직접 RPC를 통해 broadcast해야 한다. MPP는 SDK 내부에서 처리하지만 x402는 호출자 책임이다.

```rust
match x402.verify_payment_signature(&payment_header).await {
    Ok(VerifiedExactPayment::Transaction(tx)) => {
        rpc.send_transaction(&tx).await?;  // broadcast
        next.run(req).await
    }
    Ok(VerifiedExactPayment::Signature(_)) => {
        next.run(req).await  // 이미 on-chain
    }
    Err(e) => { /* 402 재챌린지 */ }
}
```

### EVM 미지원
이번 Phase는 Solana x402만 대상. EVM x402 서버 지원은 Phase 3(클라이언트 EVM 멀티체인)이 완료된 이후 별도 검토.

---

## 검증

```bash
cd rust

# 빌드
cargo check -p pay-core
cargo check -p pay-cli

# 테스트
cargo test -p pay-core

# 로컬 실행
cat > /tmp/test-x402.yml << 'EOF'
operator:
  recipient: "localnet-wallet-address"
  currency: USDC
  network: localnet
  protocol: x402
endpoints:
  - path: /v1/test
    method: GET
    metering:
      price_usd: 0.01
EOF

cargo run -- --sandbox server start /tmp/test-x402.yml

# 별도 터미널에서 호출 → x-payment-required 헤더 확인
cargo run -- --sandbox curl http://localhost:1402/v1/test
```
