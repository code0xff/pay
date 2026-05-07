# Phase 4: Runner EVM 거부 코드 제거

## 목표

`runner.rs`의 EVM 하드 거부 블록(lines 494–503)을 제거해  
x402 EVM 챌린지가 정상적으로 처리되도록 한다.  
기존 Solana MPP/x402/Session 경로는 일절 변경하지 않는다.

---

## 수정 파일: `rust/crates/core/src/client/runner.rs`

### 4-1. EVM 거부 블록 제거

현재 코드 (lines 494–503):

```rust
// 삭제 대상
// Neither protocol supports Solana — tell the user clearly.
if !mpp_challenges.is_empty() {
    return RunOutcome::PaymentRejected {
        reason: "Server requires payment but only accepts non-Solana chains \
                 (e.g. Base/EVM). This endpoint is not compatible with `pay`. \
                 Check if the provider supports Solana USDC."
            .to_string(),
        retryable: false,
        resource_url: resource_url.to_string(),
    };
}
```

이 블록을 삭제한다.  
삭제 후 코드 흐름:

```rust
    // x402 챌린지 처리 (기존)
    if let Some(challenge) = x402_challenge {
        info!(resource = resource_url, "Detected x402 payment challenge");
        return RunOutcome::X402Challenge {
            challenge: Box::new(challenge),
            resource_url: resource_url.to_string(),
        };
    }

    if let Some(challenge) = x402_siwx_challenge {
        info!(resource = resource_url, "Detected x402 sign-in challenge");
        return RunOutcome::X402SignInChallenge {
            challenge: Box::new(challenge),
            resource_url: resource_url.to_string(),
        };
    }

    // ← 여기서 기존 EVM 거부 블록이 있었음. 삭제.

    RunOutcome::UnknownPaymentRequired {
        headers: headers.to_vec(),
        resource_url: resource_url.to_string(),
    }
```

### 4-2. 삭제 근거

이 블록이 원래 하려던 일:
- `mpp_challenges`에 EVM 항목만 있고 Solana 항목이 없을 때 명시적 오류 반환

**문제**: `classify_402()`는 x402 파싱을 먼저 시도한다. EVM x402 챌린지가 있으면  
`x402_challenge`가 `Some`이 되어 이 블록에 도달하지 않는다.  
반면 `mpp_challenges`가 비어있지 않다는 것은 x402 파싱이 실패했다는 의미인데,  
이 시점에 EVM x402 챌린지가 있어도 이미 `x402_challenge`로 처리됐어야 한다.

Phase 3에서 `parse()`가 EVM `accepts` 항목도 반환하므로, 이 거부 블록은 더 이상  
필요 없다. 알 수 없는 결제 포맷은 `UnknownPaymentRequired`로 자연스럽게 처리된다.

---

## 검증

```bash
# 빌드
cargo build -p pay-core

# 기존 runner 테스트 회귀 없음 확인
cargo test -p pay-core runner

# 예상: 모든 기존 테스트 통과
# runner::tests::* (기존 테스트 모두 유지)
```

### 수동 검증 시나리오

| 시나리오 | 기대 동작 |
|---------|----------|
| Solana x402 챌린지 (기존) | `X402Challenge` — 변화 없음 |
| EVM x402 챌린지 (신규) | `X402Challenge` → Phase 3 EVM 경로 |
| EVM-only MPP 챌린지 (드문 케이스) | `UnknownPaymentRequired` (기존 거부 대신 graceful degradation) |
| 완전히 알 수 없는 포맷 | `UnknownPaymentRequired` — 변화 없음 |

---

## 다음 단계

Phase 5: [EVM 잔액 조회](./05-phase5-evm-balance.md)
