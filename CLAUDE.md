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
| `sepolia` | `eip155:11155111` | Ethereum testnet |
| `holesky` | `eip155:17000` | Ethereum testnet |
| `base-sepolia` | `eip155:84532` | Base testnet |

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

### 구현 대상 (EVM 멀티체인)
- [ ] Phase 1: `chain.rs` — ChainFamily/ChainSigner 추상화
- [ ] Phase 2: `accounts.rs` — EVM 계정 레지스트리 확장
- [ ] Phase 3: `x402.rs` + `evm.rs` — x402 멀티체인 지원
- [ ] Phase 4: `runner.rs` — EVM 거부 코드 제거
- [ ] Phase 5: `balance.rs` — EVM 잔액 조회

### 단계별 구현 가이드

| Phase | 문서 | 핵심 변경 |
|-------|------|---------|
| 1 | [docs/implementation/01-phase1-chain-abstraction.md](docs/implementation/01-phase1-chain-abstraction.md) | `chain.rs` 신규, alloy/x402-chain-eip155 의존성 추가 |
| 2 | [docs/implementation/02-phase2-account-registry.md](docs/implementation/02-phase2-account-registry.md) | `accounts.rs` EVM 필드, ephemeral 키 생성 분기 |
| 3 | [docs/implementation/03-phase3-x402-multichain.md](docs/implementation/03-phase3-x402-multichain.md) | `x402.rs` 멀티체인 파싱, `evm.rs` 신규 |
| 4 | [docs/implementation/04-phase4-runner-cleanup.md](docs/implementation/04-phase4-runner-cleanup.md) | `runner.rs` EVM 거부 블록 삭제 |
| 5 | [docs/implementation/05-phase5-evm-balance.md](docs/implementation/05-phase5-evm-balance.md) | `balance.rs` EVM 잔액 조회 추가 |

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

## 핵심 설계 원칙

1. **기존 Solana 경로 불변** — MPP, Session, Solana x402 코드 경로를 변경하지 않는다.
2. **직접 구현 최소화** — 공식 라이브러리(`x402-chain-eip155`, `alloy`)를 최대한 활용한다.
3. **enum 기반 프로토콜 디스패치** — `RunOutcome` enum 패턴을 유지하며 확장한다.
4. **하위 호환 YAML** — 기존 `accounts.yml` Solana 항목은 수정 없이 동작해야 한다.
