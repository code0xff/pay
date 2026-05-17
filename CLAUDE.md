# pay — CLAUDE.md

## 프로젝트 개요

`pay`는 HTTP 402 결제 챌린지를 자동으로 처리하는 CLI 도구입니다. AI 에이전트(Claude, Codex 등)가 stablecoin-gated API를 호출할 때 402 응답을 받으면, `pay`가 결제 서명을 처리하고 재시도합니다.

```
pay claude          # Claude Code 세션에 pay 주입
pay curl <url>      # 402 자동 처리 curl 래퍼
pay server start    # 결제 게이트웨이 서버 실행
```

---

## 디렉터리 구조

```
pay/
├── rust/                          # Rust 구현 (CLI + 서버)
│   ├── Cargo.toml                 # 워크스페이스 루트
│   └── crates/
│       ├── core/                  # 핵심 결제 로직
│       │   └── src/
│       │       ├── accounts.rs    # 계정 레지스트리 (~/.config/pay/accounts.yml)
│       │       ├── chain.rs       # [신규] 체인 추상화 (ChainFamily, ChainSigner)
│       │       ├── signer.rs      # 키 로딩 및 서명 추상화
│       │       ├── config.rs      # 앱 설정
│       │       ├── client/
│       │       │   ├── runner.rs  # 402 프로토콜 감지 (classify_402)
│       │       │   ├── mpp.rs     # MPP 클라이언트 (Solana 전용)
│       │       │   ├── x402.rs    # x402 클라이언트 (멀티체인 대상)
│       │       │   ├── evm.rs     # [신규] EVM x402 페이먼트 빌더
│       │       │   ├── balance.rs # 잔액 조회 (Solana + EVM)
│       │       │   └── session.rs # 세션 결제
│       │       └── server/
│       │           └── payment.rs # 서버 MPP 미들웨어 (Solana 전용)
│       ├── cli/                   # CLI 엔트리포인트
│       ├── keystore/              # OS 키스토어 백엔드
│       └── types/                 # 공유 타입 (Stablecoin, metering 등)
├── typescript/                    # TypeScript SDK (Solana Pay spec)
├── docs/
│   ├── ethereum-porting-review.md # 포팅 가능성 검토 문서
│   └── implementation/            # [신규] 단계별 구현 가이드
└── pdb/                           # Payment Debugger UI
```

---

## 결제 프로토콜

### 프로토콜별 역할 분리 (확정)

| 프로토콜 | 체인 지원 | 방향 |
|---------|---------|------|
| **MPP** | Solana 전용 | 현 상태 유지, 변경 없음 (EVM 스펙은 `draft-evm-charge-00` 존재하나 SDK 없어 보류) |
| **x402** | Solana + Ethereum 동시 지원 | 멀티체인 구현 대상 |
| **Session** | Solana 전용 | 현 상태 유지, 변경 없음 |

### HTTP 402 처리 흐름

```
402 응답 수신
  │
  ├── WWW-Authenticate: Payment method="solana", intent="session"
  │     └── SessionChallenge → SessionHandle (Solana MPP)
  │
  ├── WWW-Authenticate: Payment method="solana", intent="charge"
  │     └── MppChallenge → mpp::build_credential() (Solana MPP)
  │
  └── Payment-Required: base64({"accepts": [...]})
        └── X402Challenge
              ├── accepts[*].network = "solana:..." → Solana x402 서명
              └── accepts[*].network = "eip155:..."  → EVM x402 서명 [신규]
```

### x402 멀티체인 선택 로직 (구현 대상)

```
parse_x402_challenge()
  → accepts 배열 전체 파싱 (SOLANA_MAINNET 하드코딩 제거)
  → select_best_chain(accepts, store, network_override)
      1. network_override 강제 지정 시 해당 체인
      2. 구성된 지갑(accounts.yml)과 일치하는 첫 번째 accepts 항목
      3. 기본값: Solana
```

---

## 계정 레지스트리 (`~/.config/pay/accounts.yml`)

### 현재 구조 (Solana)
```yaml
version: 2
accounts:
  mainnet:
    default:
      keystore: apple-keychain
      auth_required: true
      pubkey: "7xKX...abc"          # Base58
  localnet:
    default:
      keystore: ephemeral
      secret_key_b58: "5Kj..."      # Base58 64-byte 키페어
```

### 확장 구조 (EVM 추가)
```yaml
version: 2
accounts:
  mainnet:                           # Solana mainnet (기존)
    default:
      keystore: apple-keychain
      pubkey: "7xKX...abc"
  ethereum:                          # [신규] Ethereum mainnet
    default:
      keystore: apple-keychain
      chain_family: evm
      pubkey: "0x1234...abcd"        # EIP-55 hex 주소
  base:                              # [신규] Base mainnet
    default:
      keystore: ephemeral
      chain_family: evm
      secret_key_hex: "0xdeadbeef..." # hex 32-byte 비밀키
  sepolia:                           # [신규] Sepolia testnet
    default:
      keystore: ephemeral
      chain_family: evm
      secret_key_hex: "0x..."
```

### 네트워크 슬러그 → CAIP-2 매핑

| 슬러그 | CAIP-2 | 비고 |
|-------|--------|------|
| `mainnet` | `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp` | Solana mainnet |
| `devnet` | `solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1` | Solana devnet |
| `localnet` | `solana:...` | Solana localnet |
| `ethereum` | `eip155:1` | Ethereum mainnet |
| `base` | `eip155:8453` | Base mainnet |
| `optimism` | `eip155:10` | Optimism mainnet |
| `arbitrum` | `eip155:42161` | Arbitrum One mainnet |
| `polygon` | `eip155:137` | Polygon PoS mainnet |
| `avalanche` | `eip155:43114` | Avalanche C-Chain mainnet |
| `linea` | `eip155:59144` | Linea mainnet |
| `sepolia` | `eip155:11155111` | Ethereum testnet |
| `holesky` | `eip155:17000` | Ethereum testnet |
| `base-sepolia` | `eip155:84532` | Base testnet |
| `amoy` | `eip155:80002` | Polygon Amoy testnet |

---

## 주요 의존성

### 현재 (Solana)
```toml
solana-mpp  = { git = "https://github.com/solana-foundation/mpp-sdk" }
solana-x402 = { git = "https://github.com/solana-foundation/x402-sdk" }
ed25519-dalek = "2"
bs58 = "0.5"
```

### 추가 예정 (EVM)

다음 의존성은 `evm` feature가 활성화되었을 때만 빌드 그래프에 추가된다(`pay-core/Cargo.toml`에서 `optional = true`로 선언):

```toml
# EVM x402 공식 크레이트 (Coinbase)
x402-chain-eip155 = "1.4.4"

# Ethereum 공식 Rust 라이브러리 (alloy-rs)
alloy = { version = "1.7.3", features = [
    "signer-local",    # PrivateKeySigner (secp256k1)
    "provider-http",   # HTTP JSON-RPC provider
    "eip712",          # EIP-712 structured signing
    "sol-types",       # ABI/EIP-712 타입 인코딩
    "rpc-types",       # eth_getBalance, eth_call 등
] }
```

---

## 구현 현황

### 완료
- [x] Solana MPP 클라이언트/서버
- [x] Solana x402 클라이언트 (v1/v2)
- [x] SIWX (Sign-In With Solana)
- [x] OS 키스토어 백엔드 (macOS/Linux/Windows)
- [x] 세션 결제
- [x] Payment Debugger

### 완료 (EVM 멀티체인)
- [x] Phase 1: `chain.rs` — ChainFamily/ChainSigner 추상화 (커밋 `8685a58`)
- [x] Phase 2: `accounts.rs` — EVM 계정 레지스트리 확장 (커밋 `1926aff`, 백필 `7aa1318`, `65df6c6`)
- [x] Phase 3: `x402.rs` + `evm.rs` — x402 멀티체인 지원 (커밋 `aa4ca0f`)
- [x] Phase 4: `runner.rs` — EVM 거부 코드 제거 (커밋 `b68e1cf`)
- [x] Phase 5: `balance.rs` — EVM 잔액 조회 (커밋 `37a3acc`)

### 후속 작업 (별도 트랙)

CLAUDE.md의 원래 EVM 멀티체인 플랜은 Phase 1–5로 완료되었다. 이후 발견된 작업은
모두 별도 트랙으로 관리한다.

- [x] Phase 6: x402 서버 프록시 — `protocol: x402` YAML 토글로 활성화 (Solana 내장 SDK; EVM은 `operator.facilitator_url`로 외부 facilitator 위임)
- [x] Phase 7: EVM UX 보정 — explorer 링크, send/topup 가드, 잔액 표시 통합 (커밋 `ddcc8db`)
- [x] Phase 8: EVM 라이브 통합 테스트 — `evm,network_tests` feature 하의 Sepolia/Base-Sepolia 실 RPC 검증
- [x] Phase 9: EVM 키스토어 백엔드 (라이브러리 MVP) — secp256k1 import/load/delete + signer integration. CLI `--chain-family evm --keystore X` 플래그는 follow-up
- [x] Phase 10: EVM 키스토어 CLI 진입점 (전체)
  - `pay account new --chain-family evm --network <slug>` — 신규 secp256k1 생성 + 키스토어 저장
  - `pay account destroy --network <slug>` — EVM 분기 (secp256k1 keystore 항목 삭제)
  - `pay account import --chain-family evm --network <slug> --secret-key-hex 0x...` — 기존 키 import (주소 derivation 미리 표시 후 확인)

### 단계별 구현 가이드

| Phase | 문서 | 핵심 변경 | 상태 |
|-------|------|---------|------|
| 1 | [docs/implementation/01-phase1-chain-abstraction.md](docs/implementation/01-phase1-chain-abstraction.md) | `chain.rs` 신규, alloy/x402-chain-eip155 의존성 추가 | 완료 |
| 2 | [docs/implementation/02-phase2-account-registry.md](docs/implementation/02-phase2-account-registry.md) | `accounts.rs` EVM 필드, ephemeral 키 생성 분기 | 완료 |
| 3 | [docs/implementation/03-phase3-x402-multichain.md](docs/implementation/03-phase3-x402-multichain.md) | `x402.rs` 멀티체인 파싱, `evm.rs` 신규 | 완료 |
| 4 | [docs/implementation/04-phase4-runner-cleanup.md](docs/implementation/04-phase4-runner-cleanup.md) | `runner.rs` EVM 거부 블록 삭제 | 완료 |
| 5 | [docs/implementation/05-phase5-evm-balance.md](docs/implementation/05-phase5-evm-balance.md) | `balance.rs` EVM 잔액 조회 추가 | 완료 |
| 6 | [docs/implementation/06-phase6-x402-server.md](docs/implementation/06-phase6-x402-server.md) | `pay server`에 x402 서버 프록시 추가 | 완료 |
| 7 | [docs/implementation/07-phase7-evm-ux.md](docs/implementation/07-phase7-evm-ux.md) | EVM 익스플로러 링크 + send/topup 가드 | 완료 |
| 8 | [docs/implementation/08-phase8-evm-integration-tests.md](docs/implementation/08-phase8-evm-integration-tests.md) | Sepolia 실 RPC 통합 테스트 | 완료 |
| 9 | [docs/implementation/09-phase9-evm-keystore.md](docs/implementation/09-phase9-evm-keystore.md) | secp256k1 키스토어 백엔드 (라이브러리 MVP) | 완료 |
| 10 | [docs/implementation/10-phase10-evm-keystore-cli.md](docs/implementation/10-phase10-evm-keystore-cli.md) | EVM 키스토어 CLI 진입점 (`account new`/`import`/`destroy`) | 완료 |

### 강화 트랙 (2026-05-15 감사 — 운영 전 필수)

| Phase | 문서 | 핵심 변경 | 우선순위 | 상태 |
|-------|------|---------|--------|------|
| 11 | [docs/implementation/11-phase11-evm-server-hardening.md](docs/implementation/11-phase11-evm-server-hardening.md) | EVM x402 서버: on-chain receipt 검증, nonce 중복 차단, tx_hash 헤더, 가격 fallback 제거 | **P0** | 구현 완료 |
| 12 | [docs/implementation/12-phase12-evm-payment-ux.md](docs/implementation/12-phase12-evm-payment-ux.md) | `pay send`/`topup` EVM 분기, `account/new` 후처리 분리, import 잔액 표시, facilitator 에러 매핑 | P0+P1 | 구현 완료 |
| 13 | [docs/implementation/13-phase13-evm-protocol-polish.md](docs/implementation/13-phase13-evm-protocol-polish.md) | EIP-712 도메인 on-chain 조회, typed envelope builder, decimals 테이블, v1 명시 거절, 다중 accepts | P1+P2 | 설계 완료 |
| 15 | [docs/implementation/15-phase15-evm-x402-skills-index.md](docs/implementation/15-phase15-evm-x402-skills-index.md) | `PaidEndpoint::chain_offers` 추가, probe walker EVM 확장, classify_outcome EVM-first, EVM symbol reverse lookup | P1 | 구현 완료 |
| 16 | [docs/implementation/16-phase16-evm-first-routing.md](docs/implementation/16-phase16-evm-first-routing.md) | `parse()` EVM-first hint, `select_best_chain` EVM-first priority (evm feature 시) | P0 | 구현 완료 |
| 17 | [docs/implementation/17-phase17-server-multi-accepts.md](docs/implementation/17-phase17-server-multi-accepts.md) | `operator.extra_evm_networks`, `EvmTarget` 테이블, 멀티체인 accepts 발행, 페이로드 chain_id 기반 디스패치 | P1 | 구현 완료 |
| 18 | [docs/implementation/18-phase18-client-candidate-selection.md](docs/implementation/18-phase18-client-candidate-selection.md) | `--currency` CLI 플래그, `select_best_chain` 통화 선호 (USDC > USDT 기본), narrowing 우선순위 알고리즘 | P1 | 구현 완료 |
| 19 | [docs/implementation/19-phase19-chain-token-expansion.md](docs/implementation/19-phase19-chain-token-expansion.md) | 신규 체인 (Polygon/Avalanche/Linea/Amoy) + 토큰 (Ethereum DAI/PYUSD, Polygon/Avalanche/Linea/Amoy USDC) | P2 | 구현 완료 |
| 14 | [docs/implementation/14-phase14-x402-v1-evm.md](docs/implementation/14-phase14-x402-v1-evm.md) | x402 v1 EVM 지원: `V1Eip155ExactClient`, `X-Payment` 헤더, envelope reshape (network short-name, maxAmountRequired) | P3→완료 | 구현 완료 |

---

## 빌드 및 테스트

```bash
cd rust

# 빌드
just build

# 전체 테스트
just test

# 특정 크레이트 테스트
cargo test -p pay-core

# Lint
just lint

# 로컬 실행
cargo run -- --sandbox curl https://debugger.pay.sh/mpp/quote/AAPL
```

---

## Cargo feature flags

`pay`는 멀티체인 지원을 Cargo feature 플래그로 분리한다.

| Feature        | 기본값 | 활성화하는 것 |
|----------------|--------|-------------|
| `evm`          | off    | alloy + x402-chain-eip155, ChainSigner EVM 분기, `client::evm`, balance EVM 조회, EVM 계정 생성 |
| `server`       | off    | pay-core 서버 미들웨어 (axum) |
| `gcp_kms`      | off    | Google Cloud KMS 키스토어 백엔드 |
| `vendored-openssl` | off | OpenSSL vendored 빌드 (cross-compile용) |
| `network_tests`| off    | 네트워크 의존 통합 테스트 |

### 사용 예시

```bash
# Solana 전용 (기본) — alloy 의존성 없음
cargo build

# EVM 활성화 — Ethereum, Base, Optimism, Arbitrum, Sepolia, Holesky, Base-Sepolia 지원
cargo build --features evm

# 워크스페이스 빌드 시 pay (CLI)에 EVM 활성화
cargo build --workspace --features pay/evm

# 테스트: EVM 경로 포함
cargo test --features evm
```

### 설계 원칙

1. **Solana 경로는 모든 feature 조합에서 동일하게 컴파일된다.** `evm` 플래그는 Solana 코드를 건드리지 않는다.
2. **EVM 코드는 단일 진입점을 거친다.** `#[cfg(feature = "evm")]`는 모듈 선언과 dispatch arm에만 붙고, 비즈니스 로직 내부에는 붙지 않는다.
3. **`evm` 비활성 빌드에서 EVM 네트워크 슬러그 입력 시 명확한 에러 반환** — `"Network 'ethereum' requires EVM support. Rebuild with --features evm."`
4. **`solana` feature는 존재하지 않는다.** Solana는 무조건 컴파일된다 — EVM-only `pay` 빌드 사용 사례가 없기 때문.

### 의존성 게이팅 확인

```bash
# 기본 빌드에 alloy가 없어야 함 (반드시 0)
cargo tree -p pay-core | grep -c alloy
```

---

## 핵심 설계 원칙

1. **기존 Solana 경로 불변** — MPP, Session, Solana x402 코드 경로를 변경하지 않는다.
2. **직접 구현 최소화** — 공식 라이브러리(`x402-chain-eip155`, `alloy`)를 최대한 활용한다.
3. **enum 기반 프로토콜 디스패치** — `RunOutcome` enum 패턴을 유지하며 확장한다.
4. **하위 호환 YAML** — 기존 `accounts.yml` Solana 항목은 수정 없이 동작해야 한다.
