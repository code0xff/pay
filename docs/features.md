# pay CLI 기능 명세

> HTTP 402 결제 챌린지를 자동으로 처리하는 CLI 도구. AI 에이전트(Claude, Codex)가 stablecoin-gated API를 호출할 때 결제 서명을 처리하고 재시도한다.

---

## 목차

1. [HTTP 도구 래퍼](#1-http-도구-래퍼)
2. [AI 에이전트 런처](#2-ai-에이전트-런처)
3. [계정 관리](#3-계정-관리)
4. [결제 / 송금](#4-결제--송금)
5. [API 게이트웨이 (서버 모드)](#5-api-게이트웨이-서버-모드)
6. [API 카탈로그 / Skills 관리](#6-api-카탈로그--skills-관리)
7. [카탈로그 레지스트리 (공급자용)](#7-카탈로그-레지스트리-공급자용)
8. [설정 및 초기화](#8-설정-및-초기화)
9. [글로벌 플래그](#9-글로벌-플래그)
10. [결제 프로토콜](#10-결제-프로토콜)
11. [계정 레지스트리](#11-계정-레지스트리)
12. [지원 스테이블코인](#12-지원-스테이블코인)
13. [환경 변수](#13-환경-변수)

---

## 1. HTTP 도구 래퍼

402 응답을 자동으로 감지하고 결제를 처리한 뒤 재시도한다. 사용자 입장에서는 기존 도구와 동일하게 사용하면 된다.

### `pay curl <args>`

curl 드롭인 대체.

- `-D <tempfile>` 로 응답 헤더를 캡처하여 402 감지
- MPP / x402 / Session 프로토콜 자동 판별 후 서명 생성
- stdout / stderr / stdin 그대로 상속 → 스크립트 호환
- 나머지 인자는 모두 curl 로 전달

### `pay wget <args>`

wget 래퍼. curl 과 동일한 방식으로 402를 처리한다.

### `pay http <args>`

HTTPie(`http`) 래퍼. request-item 문법을 그대로 지원한다.

### `pay fetch <url> [-H "Key: Value"]`

외부 도구 없이 동작하는 내장 HTTP 클라이언트.

| 옵션 | 설명 |
|------|------|
| `<url>` | 요청할 URL |
| `-H, --header <HEADER>` | 추가 헤더 (반복 가능) |

바이너리 응답 처리, Content-Type 헤더 출력 포함.

---

## 2. AI 에이전트 런처

pay MCP 서버를 에이전트 세션에 주입하여 402 결제를 자동화한다.

### `pay claude [args]`

Claude Code 실행 시 pay MCP 서버를 자동 주입.

- `--mcp-config` 에 pay 바이너리 + `mcp` 서브커맨드를 JSON으로 구성
- `--strict-mcp-config` + 허용 MCP 툴 목록 적용
- 결제 관련 시스템 프롬프트 자동 삽입
- `PAY_ACTIVE_ACCOUNT`, `PAY_RPC_URL`, `PAY_NETWORK_ENFORCED`, `PAY_DEBUGGER_PROXY` 환경 변수 전달

**허용 MCP 툴 목록:**

| 툴 | 설명 |
|----|------|
| `mcp__pay__curl` | 402 처리 curl |
| `mcp__pay__search_catalog` | 카탈로그 검색 |
| `mcp__pay__list_catalog` | 카탈로그 목록 |
| `mcp__pay__get_catalog_entry` | 카탈로그 항목 조회 |
| `mcp__pay__get_balance` | 잔액 조회 |
| `mcp__pay__topup` | 충전 |
| `mcp__pay__create_skill` | 스킬 생성 |

### `pay codex [args]`

Codex에 동일하게 주입. 허용 MCP 툴 목록은 claude와 동일.

---

## 3. 계정 관리

계정 정보는 `~/.config/pay/accounts.yml`에 저장된다.

### `pay account new`

새 계정(키페어)을 생성하고 OS 키스토어에 저장.

```
pay account new [--backend BACKEND] [--vault VAULT] [--account ACCOUNT]
```

| 옵션 | 설명 |
|------|------|
| `--backend` | 키스토어 백엔드 선택 |
| `--vault` | 1Password 볼트 이름 |
| `--account` | 1Password 계정 UUID |

### `pay account import <json-file>`

Solana JSON 키페어 파일로 계정을 임포트.

### `pay account list` (별칭: `ls`)

등록된 계정 목록과 스테이블코인 잔액을 출력.

- 네트워크 → 계정명 → 공개키 → 잔액(SOL + 스테이블코인)
- 활성 계정 강조 표시

### `pay account default <name>`

지정한 계정을 해당 네트워크의 기본 계정으로 설정.

### `pay account remove <name>` (별칭: `rm`, `destroy`)

계정을 영구 삭제. OS 키스토어에서도 함께 제거.

### `pay account export <name> [--output path.json]`

계정을 JSON 키페어 파일로 내보내기 (백업/마이그레이션용).

---

## 4. 결제 / 송금

### `pay send <amount> <recipient>`

스테이블코인을 직접 전송.

```
pay send <amount> <recipient> [--currency COIN] [--memo TEXT] [--memo-hex HEX] [--fee-within]
```

| 옵션 | 설명 |
|------|------|
| `<amount>` | 소수점 금액(예: `1.25`) 또는 `max` |
| `<recipient>` | Base-58 공개키 또는 계정명 |
| `--currency` | 스테이블코인 심볼 (잔액이 1종류면 자동 선택) |
| `--memo` | UTF-8 메모 |
| `--memo-hex` | Hex 인코딩 메모 (`--memo`와 상호 배타적) |
| `--fee-within` | 수수료를 금액에서 차감 |

### `pay topup [--account ADDR] [--sandbox]`

MoonPay를 통해 Venmo / PayPal / 모바일 지갑에서 충전.

1. 대상 계정 공개키 확인
2. 브라우저 → MoonPay 플로우 실행
3. SOL 잔액 폴링으로 입금 감지
4. 트랜잭션 서명 출력

---

## 5. API 게이트웨이 (서버 모드)

YAML 스펙 기반 HTTP 402 게이트웨이 프록시. 미터링된 엔드포인트에 MPP 챌린지를 발행하고 결제 서명을 검증한다.

> **현재 지원 프로토콜: MPP (Solana 전용)**

### `pay server start <spec-file>`

```
pay server start <spec.yml> [옵션들]
```

| 옵션 | 기본값 | 설명 |
|------|--------|------|
| `--bind` | `0.0.0.0:1402` | 수신 주소 |
| `--recipient` | — | 결제 수신 지갑 주소 |
| `--currency` | USDC | 결제 통화 |
| `--rpc-url` | — | 결제 검증용 RPC |
| `--debugger` | false | Payment Debugger UI 동시 실행 |
| `--otlp-sidecar` | — | OpenTelemetry OTLP 엔드포인트 |
| `--openapi` | — | OpenAPI 3 / Google Discovery JSON 경로 |
| `--public-url` | — | OpenAPI 스펙의 base URL 오버라이드 |

**미들웨어 동작:**

1. 미터링 설정이 있는 엔드포인트 → 402 + MPP 챌린지 발행
2. `Authorization` 헤더가 있으면 결제 서명 검증
3. 검증 통과 시 upstream으로 포워딩, `X-Payment-Receipt` 헤더 첨부
4. 미터링 설정 없는 엔드포인트 → 그대로 통과

### `pay server demo`

데모 서버 + Payment Debugger UI를 함께 실행. 로컬 탐색용.

### `pay server scaffold`

샘플 YAML 스펙 파일을 생성. 엔드포인트 라우팅, 미터링, 통화 설정 템플릿 포함.

---

## 6. API 카탈로그 / Skills 관리

공급자 API를 검색하고 로컬 캐시로 관리한다.

### `pay skills search <query>`

공급자 FQN, 이름, 설명을 로컬 캐시에서 검색.

### `pay skills endpoints <provider-fqn>`

특정 공급자의 엔드포인트 목록, 메서드, 결제 티어, 설명 출력.

### `pay skills add <github-org/repo-or-url>`

공급자 소스(GitHub 리포 또는 커스텀 카탈로그 URL) 등록.

### `pay skills remove <source>` / `pay skills list` / `pay skills update [--force]`

소스 관리 및 로컬 캐시 갱신.

### `pay install <github-org/repo>`

`pay skills add`의 단축 명령.

---

## 7. 카탈로그 레지스트리 (공급자용)

pay.sh에 API를 등록하려는 공급자가 사용하는 명령어.

### `pay catalog scaffold <openapi-url>`

OpenAPI 스펙 URL에서 `PAY.md` 공급자 메타데이터 파일을 자동 생성.

### `pay catalog check`

YAML 검증 (CI 환경에서 쓰기 없이 실행 가능).

```
pay catalog check [--file PATH] [--changed-from REF]
```

| 옵션 | 설명 |
|------|------|
| `--file` | 단일 파일 검증 |
| `--changed-from <ref>` | Git diff로 변경된 파일만 검증 |
| (없음) | 전체 레지스트리 검증 |

검증 항목: frontmatter 형식, OpenAPI 유효성, Solana 네트워크 요구사항, probe 테스트.

### `pay catalog build`

검증 통과 후 `dist/skills.json` + `dist/providers/*.json` 빌드. main 브랜치 CI에서 실행.

---

## 8. 설정 및 초기화

### `pay setup [--force] [--backend BACKEND] [--update]`

계정 생성 + topup을 한 번에 처리하는 온보딩 커맨드.

| 옵션 | 설명 |
|------|------|
| `--force` | 기존 계정 교체 |
| `--backend` | 키스토어 타입 강제 지정 |
| `--update` | 계정 생성 없이 MCP 설정 + 에이전트 스킬만 재설치 |

### `pay whoami [--account NAME]`

시스템 사용자 + 활성 pay 계정 + 스테이블코인 잔액 출력.

### `pay mcp`

MCP 서버를 단독으로 실행 (Claude Code 내부에서 사용).

### `pay help`

전체 결제 프로토콜 가이드를 출력.

---

## 9. 글로벌 플래그

모든 커맨드에서 사용 가능.

| 플래그 | 설명 |
|--------|------|
| `--sandbox` / `--dev` | localnet + Surfpool RPC 강제 |
| `--local` | localnet + localhost RPC 강제 |
| `--mainnet` | mainnet 강제 |
| `--account <NAME>` | 기본 계정 대신 지정 계정 사용 |
| `--no-dna` | 머신 리더블 JSON 출력 (색상/포맷 없음) |
| `--verbose` / `-v` | 트레이싱 로그 + 결제 상세 출력 |
| `--debugger` | Payment Debugger 프록시(:1402) 동시 실행 |
| `--yolo-upto <AMOUNT>` | 자동 결제 한도 (숨김 플래그, 단위: micro stablecoin) |

---

## 10. 결제 프로토콜

### MPP (Machine Payments Protocol)

- **감지**: `WWW-Authenticate: Payment method="solana", intent="charge"`
- **서명**: Ed25519 (Solana 키페어)
- **클라이언트**: `pay_core::mpp::build_credential()`
- **서버**: `pay_core::server::payment` 미들웨어

### MPP Session

- **감지**: `WWW-Authenticate: Payment method="solana", intent="session"`
- **모드**:
  - Pull mode (기본): 오퍼레이터가 on-chain Fiber 채널에서 인출. 설정 경량.
  - Push mode (폴백): 사용자가 on-chain 채널에 직접 예치.
- **클라이언트**: `pay_core::session`

### x402 (Cross-Chain 402)

- **감지**: `Payment-Required: base64({"accepts": [...]})`
- **현재 지원 네트워크**: Solana mainnet / devnet
- **개발 중**: EVM(Ethereum, Base 등) 멀티체인 지원 (Phase 1-5)
- **클라이언트**: `pay_core::x402::build_payment()`
- **서버사이드**: 미지원 (SDK 부재)

### SIWX (Sign-In With X)

- 결제 없는 인증 전용 플로우
- `Payment-Required` 헤더에 결제 필드 없이 SIWX 확장만 포함된 경우 처리
- **클라이언트**: `pay_core::x402::build_siwx_auth_header()`

### 프로토콜 지원 매트릭스

| 프로토콜 | 클라이언트 | 서버 | 체인 |
|---------|:---------:|:----:|------|
| MPP | ✅ | ✅ | Solana |
| MPP Session | ✅ | ✅ | Solana |
| x402 | ✅ | ❌ | Solana (EVM 개발 중) |
| SIWX | ✅ | ❌ | Solana |

---

## 11. 계정 레지스트리

`~/.config/pay/accounts.yml` 에 저장. YAML v2 스키마.

### 키스토어 백엔드

| 백엔드 | 플랫폼 | 비밀키 저장 | 인증 게이트 | 용도 |
|--------|--------|------------|------------|------|
| `apple-keychain` | macOS | Secure Enclave | Touch ID (선택) | 프로덕션 |
| `gnome-keyring` | Linux | libsecret | polkit (선택) | 프로덕션 |
| `windows-hello` | Windows | TPM | Windows Hello (선택) | 프로덕션 |
| `1password` | 전체 | 1Password 볼트 | 1Password 잠금 | 팀 공유 |
| `file` | 전체 | 비암호화 JSON | 없음 | 개발/테스트 |
| `ephemeral` | 전체 | YAML 인라인 | 없음 | 샌드박스/localnet |

### 네트워크 슬러그 → CAIP-2 매핑

| 슬러그 | CAIP-2 | 비고 |
|--------|--------|------|
| `mainnet` | `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp` | Solana mainnet |
| `devnet` | `solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1` | Solana devnet |
| `localnet` | `solana:...` | 로컬 밸리데이터 |
| `ethereum` | `eip155:1` | Ethereum mainnet |
| `base` | `eip155:8453` | Base mainnet |
| `sepolia` | `eip155:11155111` | Ethereum testnet |
| `holesky` | `eip155:17000` | Ethereum testnet |
| `base-sepolia` | `eip155:84532` | Base testnet |

---

## 12. 지원 스테이블코인

| 심볼 | 발행사 | mainnet | devnet |
|------|--------|:-------:|:------:|
| USDC | Circle | ✅ | ✅ |
| USDT | Tether | ✅ | ❌ |
| PYUSD | PayPal | ✅ | ✅ |
| CASH | — | ✅ | ❌ |
| USDG | — | ✅ | ❌ |

---

## 13. 환경 변수

| 변수 | 설명 |
|------|------|
| `PAY_AUTO_PAY` | 프롬프트 없이 자동 결제 (`true`/`false`) |
| `PAY_RPC_URL` | RPC URL 오버라이드 (전체 커맨드) |
| `PAY_MAINNET_RPC_URL` | mainnet 잔액 조회용 RPC 오버라이드 |
| `PAY_NETWORK_ENFORCED` | 네트워크 슬러그 강제 지정 (MCP 내부) |
| `PAY_ACTIVE_ACCOUNT` | 활성 계정명 오버라이드 (MCP 내부) |
| `PAY_DEBUGGER_PROXY` | Payment Debugger 프록시 URL |
| `PAY_API_URL` | pay-api 호스트 (기본: `https://api.gateway-402.com`) |
| `PAY_LOG_FORMAT` | 로그 형식 (`text` / `json`) |
