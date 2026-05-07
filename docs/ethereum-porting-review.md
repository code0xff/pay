# Ethereum 포팅 가능성 검토

> **결론 요약**: 기술적으로 포팅 가능하나, 대규모 재구현이 필요합니다.  
> HTTP 402 challenge-response 흐름, 계정 관리 구조, CLI/서버 아키텍처는 체인 독립적이어서 재사용 가능합니다.  
> 반면 서명 알고리즘, 트랜잭션 모델, 토큰 표준, 결제 프로토콜 SDK는 Solana에 강하게 결합되어 전면 교체가 필요합니다.

---

## 1. 프로젝트 구조 개요

```
pay/
├── rust/crates/
│   ├── core/      # 핵심 결제 로직 (MPP, x402, 서명, 잔액 조회)
│   ├── cli/       # CLI 명령어 (send, topup, account, server 등)
│   └── pdb/       # Payment Debugger 서버
└── typescript/packages/solana-pay/
    └── core/      # Solana Pay URL 인코딩/파싱 (TS SDK)
```

핵심 Solana 의존성은 `rust/Cargo.toml`에 집중됩니다:

```toml
solana-mpp  = { git = "https://github.com/solana-foundation/mpp-sdk" }
solana-x402 = { git = "https://github.com/solana-foundation/x402-sdk" }
solana-hash, solana-instruction, solana-message,
solana-pubkey, solana-signature, solana-transaction, ...
```

---

## 2. Solana 종속 컴포넌트 분류

### 2.1 서명·키 관리 레이어 — **전면 교체 필요**

| 항목 | Solana 현재 구현 | Ethereum 대체안 |
|------|----------------|----------------|
| 서명 알고리즘 | ed25519 (`ed25519-dalek`) | secp256k1 (`k256` or `ethers-rs`) |
| 공개키 형식 | Base58 32 bytes | 0x-prefixed hex 20 bytes (EIP-55) |
| 키페어 형식 | 64 bytes (secret \|\| public), Base58 | 32 bytes 비밀키, hex |
| Signer 트레잇 | `SolanaSigner` (solana-mpp/x402 내장) | `ethers::Signer` 등 |

**관련 코드 위치:**
- `rust/crates/core/src/signer.rs` — `MemorySigner`, `load_signer_*` 함수군 전체
- `rust/crates/core/src/accounts.rs` — `secret_key_b58`, `generate_ephemeral_account()`, `parse_private_key_string()`
- `rust/crates/core/src/keystore.rs` — OS 키스토어 백엔드 (로직은 재사용 가능, 저장 포맷만 교체)

---

### 2.2 결제 프로토콜 레이어 — **가장 큰 작업 범위**

현재 두 프로토콜을 모두 지원합니다:

#### MPP (Machine Payments Protocol)
- `solana-mpp` SDK가 Solana 전용으로 설계됨
- Ethereum용 MPP SDK가 존재하지 않음 → **직접 구현 또는 프로토콜 교체 필요**

**관련 코드 위치:**
- `rust/crates/core/src/client/mpp.rs`
  - `build_credential()` — Solana RPC + `build_credential_header()` 호출
  - `select_challenge_by_balance()` — SPL 토큰 잔액 기반 챌린지 선택
  - `check_client_network_intent()` — Surfpool blockhash 검증 (Solana 전용)
  - `SURFPOOL_BLOCKHASH_PREFIX` 상수 제거 필요
- `rust/crates/core/src/server/payment.rs`
  - `solana_mpp::server::Mpp`, `charge_challenge_response()`, `handle_charge_authorization()`
  - `solana_mpp::protocol::solana::Split` — 결제 분배 구조

#### x402
- `solana-x402` SDK도 Solana 전용이나, **CAIP-2 체인 ID 체계를 이미 사용** 중
- Coinbase의 x402 레퍼런스 구현에는 EVM 지원이 포함되어 있음 → **가장 현실적인 포팅 경로**
- x402 v2는 `PaymentRequirements.network` 필드에 CAIP-2 ID를 사용하여 멀티체인 확장 여지가 있음

**관련 코드 위치:**
- `rust/crates/core/src/client/x402.rs`
  - `normalize_network()` — Solana CAIP-2 제네시스 해시를 슬러그로 변환
  - `siwx_chain_id_for_network()` — Solana 전용 chain ID 매핑
  - `build_payment()` — `RpcClient`, `build_payment_header_v1/v2()` 호출
  - `SiwxExtension` — SIWS(Sign-In With Solana) → **SIWE(Sign-In With Ethereum)로 교체 필요**

---

### 2.3 트랜잭션 모델 레이어 — **개념 수준의 차이 존재**

| 항목 | Solana | Ethereum |
|------|--------|----------|
| 유효기간 | `recentBlockhash` (약 ~150 블록) | `nonce` (단조 증가) |
| 수수료 | fee payer 계정, 고정 lamport | `gasPrice` × `gasLimit`, 시장 동적 |
| 트랜잭션 직렬화 | `bincode` | RLP 인코딩 |
| 토큰 전송 | SPL Token Program (ATA 주소 파생 필요) | ERC-20 `transfer()` 직접 호출 |
| 멀티 전송(splits) | 단일 트랜잭션 내 다중 instruction | ERC-20 단일 call 또는 별도 컨트랙트 |

**관련 코드 위치:**
- `rust/Cargo.toml` — `solana-hash`, `solana-instruction`, `solana-message`, `solana-transaction`, `bincode`
- `rust/crates/core/src/client/mpp.rs` — `embedded_blockhash`, `recentBlockhash` 처리

---

### 2.4 토큰·잔액 레이어 — **주소 체계 전면 교체**

**관련 코드 위치:**
- `rust/crates/core/src/client/balance.rs`
  - `get_balances()` — Solana JSON-RPC `getBalance`, `getMultipleAccounts` 호출
  - `fetch_stablecoins_via_api()` — pay-api `/v1/balance/stablecoins` (Solana 기반 서비스)
  - `infer_network()` — "surfnet", "surfpool" 등 Solana 특화 키워드

- `pay_types::Stablecoin` (외부 크레이트)
  - Solana mainnet 민트 주소 하드코딩: `USDC_MAINNET`, `USDT_MAINNET`, `CASH_MAINNET`, `USDG_MAINNET`
  - Ethereum 포팅 시 ERC-20 컨트랙트 주소 레지스트리로 교체 필요

Ethereum RPC 대체 매핑:

| Solana RPC 메서드 | Ethereum RPC 대체 |
|-----------------|-----------------|
| `getBalance` | `eth_getBalance` |
| `getMultipleAccounts` | `eth_call` (ERC-20 `balanceOf`) × N |
| `getSignaturesForAddress` | `eth_getLogs` (Transfer event 필터링) |

---

### 2.5 네트워크 식별자 — **슬러그 체계 교체**

**관련 코드 위치:**
- `rust/crates/core/src/accounts.rs` — `MAINNET_NETWORK = "mainnet"`, `is_lazy_ephemeral_network()`
- `rust/crates/core/src/client/mpp.rs` — `normalize_network()`, `is_sandbox_network()`
- `rust/crates/core/src/client/x402.rs` — CAIP-2 Solana 체인 ID 상수들

Ethereum은 체인 ID 숫자 체계 사용: mainnet=1, sepolia=11155111, holesky=17000 등

---

### 2.6 TypeScript SDK — **Solana Pay 스펙 전체가 Solana 전용**

`typescript/packages/solana-pay/core/src/` 전체가 교체 대상:

| 파일 | 내용 | 비고 |
|------|------|------|
| `types.ts` | `Address` (`@solana/kit`), `SPLToken`, `Reference` | EIP-55 주소 타입으로 교체 |
| `encodeURL.ts` | `solana:` URL 스킴 | EIP-681 (`ethereum:`) 또는 WalletConnect URI |
| `parseURL.ts` | `solana:` URL 파싱 | 동일 |
| `createTransfer.ts` | SPL Token / SystemProgram.Transfer 트랜잭션 생성 | `ethers.js` ERC-20 트랜잭션으로 교체 |
| `validateTransfer.ts` | Solana RPC `getTransaction` 검증 | Ethereum receipt 검증 |
| `findReference.ts` | `getSignaturesForAddress` | `eth_getLogs` + Transfer event |
| `watchReference.ts` | 폴링 기반 Solana 트랜잭션 감시 | `eth_subscribe` 또는 폴링 |
| `fetchTransaction.ts` | Solana 트랜잭션 fetch | Ethereum tx receipt fetch |

---

## 3. 재사용 가능한 컴포넌트 (체인 독립적)

다음은 Ethereum 포팅 시에도 그대로 활용할 수 있습니다:

| 컴포넌트 | 위치 | 설명 |
|---------|------|------|
| HTTP 402 challenge-response 흐름 | `client/runner.rs`, `client/fetch.rs` | 프로토콜 독립적 구조 |
| 계정 관리 YAML 스키마 | `accounts.rs` | `Keystore` 열거형, `AccountsFile` 구조 (저장 포맷 교체만 필요) |
| OS 키스토어 백엔드 | `signer.rs` | Apple Keychain, GNOME Keyring, Windows Hello, 1Password — 포맷만 교체 |
| 서버 미터링·프록시 구조 | `server/metering.rs`, `server/proxy.rs` | 결제 검증 부분만 교체 |
| Payment Debugger UI | `pdb/` | 체인 독립적 시각화 |
| CLI 명령어 구조 | `cli/src/commands/` | `account`, `skills`, `server`, `curl` 등 구조 재사용 |
| 잔액 조회 추상화 | `client/balance.rs` 상위 구조 | `AccountBalances`, `ReceivedFunds` 구조체 재사용 가능 |

---

## 4. 포팅 난이도 매트릭스

| 항목 | 난이도 | 이유 |
|------|--------|------|
| MPP 프로토콜 | 🔴 매우 높음 | Ethereum용 MPP SDK 없음, 프로토콜 재설계 필요 |
| x402 프로토콜 | 🟡 중간 | Coinbase x402의 EVM 지원 활용 가능하나 SDK 교체 필요 |
| 서명·키 관리 | 🟡 중간 | ed25519 → secp256k1, 주소 포맷 변경 |
| 트랜잭션 빌딩 | 🔴 높음 | blockhash/nonce, gas, ATA 개념 차이 |
| 토큰 잔액 조회 | 🟡 중간 | RPC 메서드 교체, ERC-20 ABI 필요 |
| TypeScript SDK | 🔴 높음 | Solana Pay 스펙 전체가 Solana 전용, EIP-681 기반 재작성 필요 |
| 계정 관리 YAML | 🟢 낮음 | 구조 재사용, 저장 포맷만 수정 |
| OS 키스토어 백엔드 | 🟢 낮음 | 로직 재사용, 바이트 포맷만 수정 |
| 서버/프록시 구조 | 🟢 낮음 | 결제 검증 콜백만 교체 |

---

## 5. 권장 포팅 전략

### 전략 A: x402 우선 포팅 (권장)

x402 프로토콜은 이미 CAIP-2 체인 ID를 사용하며, Coinbase의 레퍼런스 구현에 EVM 지원이 포함되어 있습니다.

1. `solana-x402` SDK → EVM 호환 x402 SDK로 교체
2. 서명 알고리즘: `ed25519-dalek` → `k256` (secp256k1)
3. RPC 레이어: Solana JSON-RPC → Ethereum JSON-RPC (`ethers-rs` 또는 `alloy`)
4. 토큰 레지스트리: SPL 민트 주소 → ERC-20 컨트랙트 주소
5. SIWX: SIWS(Sign-In With Solana) → SIWE(Sign-In With Ethereum, EIP-4361)
6. TypeScript SDK: `solana:` URL → EIP-681 또는 WalletConnect URI

### 전략 B: 멀티체인 추상화 레이어 도입

`PaymentProtocol` 트레잇을 정의하여 Solana/Ethereum 구현을 교체 가능하게 구성:

```rust
trait ChainSigner: Send + Sync {
    fn sign(&self, message: &[u8]) -> Vec<u8>;
    fn address(&self) -> String;
}

trait PaymentProtocol: Send + Sync {
    fn build_challenge(&self, amount: &str) -> Result<String>;
    fn verify_credential(&self, credential: &str) -> Result<Receipt>;
}
```

이 방식은 코드베이스를 크게 건드리지 않고 Ethereum 구현을 플러그인처럼 추가할 수 있지만, 초기 리팩터링 비용이 높습니다.

---

## 6. 핵심 검토 파일 목록

| 파일 | 검토 이유 | 우선순위 |
|------|----------|---------|
| `rust/crates/core/src/client/mpp.rs` | MPP 챌린지 생성/검증 전체 | 🔴 필수 |
| `rust/crates/core/src/client/x402.rs` | x402 챌린지 생성/검증 전체 | 🔴 필수 |
| `rust/crates/core/src/signer.rs` | 키 로딩 및 서명 추상화 | 🔴 필수 |
| `rust/crates/core/src/accounts.rs` | 계정 저장 포맷, 에페머럴 생성 | 🟡 중요 |
| `rust/crates/core/src/client/balance.rs` | RPC 잔액 조회 | 🟡 중요 |
| `rust/crates/core/src/server/payment.rs` | 서버측 결제 미들웨어 | 🟡 중요 |
| `rust/Cargo.toml` | Solana 의존성 전체 목록 | 🔴 필수 |
| `typescript/packages/solana-pay/core/src/` | TS SDK 전체 | 🟡 중요 |
| `typescript/packages/solana-pay/spec/SPEC.md` | 프로토콜 스펙 참조 | 🟢 참고 |

---

*작성일: 2026-05-07*
