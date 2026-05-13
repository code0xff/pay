# Phase 7: EVM UX 보정

## 목표

Phase 1–5에서 도입된 EVM 멀티체인 지원의 사용자 경험 누락분을 보정한다.
`pay account list`, `pay whoami`는 이미 EVM 잔액을 표시하지만, 익스플로러
링크와 일부 명령어가 여전히 Solana 가정에 묶여 있다.

**원칙**

1. 기존 Solana 호출 경로의 동작은 절대 변경하지 않는다.
2. 새 분기는 `pay_core::accounts::is_evm_network_family(network) || account.is_evm()`로 판별한다.
3. 출력 포맷은 기존 `print_balances`/`format_account_header` 컴포넌트를 재사용한다.

---

## 7-1. EVM 익스플로러 링크

### 현재 동작

`crates/cli/src/components/account.rs::explorer_link(pubkey, rpc_url)`:

- 항상 `https://explorer.solana.com/address/<pubkey>/tokens`를 반환.
- 비-mainnet RPC URL인 경우 `?cluster=custom&customUrl=<rpc>`를 덧붙여 사용자
  지정 클러스터에 연결.
- EVM 계정에서는 잘못된 도메인 + 잘못된 주소 형식을 가리킨다.

### 변경 설계

`explorer_link` 시그니처를 네트워크 슬러그를 받도록 확장하고, EVM 네트워크에
대해서는 체인별 익스플로러 URL을 반환한다.

```rust
// crates/cli/src/components/account.rs
pub fn explorer_link(pubkey: Option<&str>, network: &str, rpc_url: &str) -> String {
    match pubkey {
        Some(pk) if !pk.is_empty() => {
            let url = if pay_core::accounts::is_evm_network_family(network) {
                evm_explorer_url(network, pk)
            } else {
                solana_explorer_url(network, pk, rpc_url)
            };
            format!("\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\", "balance ↗".dimmed())
        }
        _ => "—".dimmed().to_string(),
    }
}

fn evm_explorer_url(network: &str, address: &str) -> String {
    let base = match network {
        "ethereum"      => "https://etherscan.io/address",
        "base"          => "https://basescan.org/address",
        "optimism"      => "https://optimistic.etherscan.io/address",
        "arbitrum"      => "https://arbiscan.io/address",
        "sepolia"       => "https://sepolia.etherscan.io/address",
        "holesky"       => "https://holesky.etherscan.io/address",
        "base-sepolia"  => "https://sepolia.basescan.org/address",
        _               => "https://etherscan.io/address",
    };
    format!("{base}/{address}")
}

fn solana_explorer_url(network: &str, pubkey: &str, rpc_url: &str) -> String {
    let base = format!("https://explorer.solana.com/address/{pubkey}/tokens");
    // mainnet 슬러그 우선 — RPC URL에 "mainnet"이 포함되어 있지 않아도
    // 슬러그 기준으로 cluster 파라미터를 결정해야 base/sepolia 등 EVM 슬러그가
    // Solana mainnet 익스플로러로 잘못 라우팅되는 사고를 막을 수 있다.
    if network == "mainnet" || rpc_url.contains("mainnet") {
        base
    } else {
        let encoded = percent_encode_rpc(rpc_url);
        format!("{base}?cluster=custom&customUrl={encoded}")
    }
}
```

### 호출처 변경

| 파일 | 함수 | 변경 |
|---|---|---|
| `components/account.rs` | `print_balance_unavailable` | `network: &str` 인자 추가, `explorer_link`에 전달 |
| `components/account.rs` | `format_balance_display` | 위와 동일 |
| `commands/account/list.rs` | `print_account_list` | 내부 루프에서 이미 `network`를 알고 있으므로 그대로 전달 |
| `commands/account/list.rs` | `fetch_balance` (top-level) | 호출 사이트가 없으면 그대로 두고, 있다면 EVM 분기 추가 검토 |
| `commands/whoami.rs` | `WhoamiCommand::run` | `network` 변수를 `print_balance_unavailable`에 전달 |

### 테스트

`components/account.rs` 하단에 단위 테스트 추가:

```rust
#[test]
fn explorer_link_for_evm_networks_uses_correct_block_explorer() {
    let link_eth = explorer_link(Some("0xabc"), "ethereum", "");
    assert!(link_eth.contains("etherscan.io/address/0xabc"));

    let link_base = explorer_link(Some("0xabc"), "base", "");
    assert!(link_base.contains("basescan.org/address/0xabc"));

    let link_sepolia = explorer_link(Some("0xabc"), "sepolia", "");
    assert!(link_sepolia.contains("sepolia.etherscan.io/address/0xabc"));
}

#[test]
fn explorer_link_for_solana_mainnet_omits_cluster_param() {
    let link = explorer_link(Some("ABC"), "mainnet", "https://api.mainnet-beta.solana.com");
    assert!(link.contains("explorer.solana.com/address/ABC/tokens"));
    assert!(!link.contains("cluster=custom"));
}

#[test]
fn explorer_link_for_solana_devnet_appends_cluster_param() {
    let link = explorer_link(Some("ABC"), "devnet", "https://api.devnet.solana.com");
    assert!(link.contains("cluster=custom"));
}
```

---

## 7-2. send/topup 가드

### 배경

`pay send`, `pay topup`은 Solana 전용이지만 `--network sepolia` 등 EVM 슬러그를
받으면 Solana 코드 경로(`solana_keychain::SolanaSigner`, pay-api 통한 stablecoin
전송)로 진입한다. 다음 중 하나가 발생:

- pay-api 잔액 조회가 빈 결과를 반환 → 사용자에게는 "0 USDC" 또는 "api offline"으로 보임.
- 서명 시도 단계에서 `MemorySigner::from_bytes`가 실패 → 혼란스러운 에러.

### 변경 설계

명령어 진입점에서 명시적으로 거부한다.

```rust
// crates/cli/src/commands/send.rs, topup.rs 진입 부근
if pay_core::accounts::is_evm_network_family(network) {
    return Err(pay_core::Error::Config(format!(
        "`pay {sub}` is not yet supported on EVM networks (got `{network}`). \
         For now use a wallet like MetaMask to manage EVM balances; \
         `pay account list` shows them read-only."
    )));
}
```

`sub`은 `"send"` 또는 `"topup"`. 메시지는 사용자가 다음 행동을 알 수 있도록 명확하게.

### 영향 분석

| 명령어 | 현재 동작 | 변경 후 |
|---|---|---|
| `pay send --network sepolia` | 혼란스러운 에러 또는 0 잔액 | 명시적 거부 + 안내 |
| `pay topup --network sepolia` | 동일 | 명시적 거부 |
| `pay send` (네트워크 미지정) | mainnet(=Solana) 가정 — 변화 없음 | 변화 없음 |
| `pay topup` (네트워크 미지정) | mainnet(=Solana) 가정 — 변화 없음 | 변화 없음 |

### 테스트

`commands/send.rs`, `commands/topup.rs` 하단 단위 테스트:

```rust
#[test]
fn send_rejects_evm_network_with_clear_error() {
    // SendCommand::run(...) 호출 또는 가드 함수 직접 호출
    let err = guard_solana_only("send", "sepolia").unwrap_err();
    assert!(err.to_string().contains("not yet supported on EVM"));
}
```

---

## 7-3. 잔액 표시 통합

### 배경

`pay account list`와 `pay whoami`가 EVM 잔액을 표시하지만:

- ETH 잔액의 단위 표시가 일관되지 않을 수 있음 (`format_balance_display`는 USDC
  하드코딩).
- `print_balances`의 라벨 폭(`{:<6}`)이 "USDT"는 맞지만 "ETH"도 정렬되도록 확인 필요.

### 변경 설계

1. `format_balance_display` (account/list.rs:209)에서 USDC 우선 정렬 후 "다른
   토큰"은 그대로 출력하므로 ETH는 자동 처리됨. **변경 불필요** — 동작 확인만.

2. `print_balances`의 `{:<6}` 폭은 USDC/USDT/ETH 모두 4자 이내라 그대로 OK.

3. EVM 계정의 mainnet 가드:
   - `print_topup_note()`는 Solana mainnet에서만 호출되므로 EVM 계정에 영향 없음.
   - 단, `any_mainnet_funded` 변수가 모든 mainnet(슬러그 일치)을 대상으로 동작.
     EVM은 `ethereum` 슬러그라 영향 없음. 확인만 하고 변화 없음.

### 검증

- 수동: 로컬에서 `accounts.yml`에 EVM 항목 추가 후 `pay account list` 실행 →
  ETH/USDC 라인이 정렬되어 출력되는지 확인.

---

## 검증 절차 (전체)

```bash
# 빌드
cargo build -p pay --features evm

# 단위 테스트
cargo test -p pay --features evm explorer_link
cargo test -p pay --features evm send_rejects_evm
cargo test -p pay --features evm topup_rejects_evm

# 기존 Solana 경로 회귀 없음
cargo test --features evm,server --workspace
```

### 수동 검증 시나리오

| 시나리오 | 기대 동작 |
|---|---|
| Solana mainnet 계정에서 `pay account list` | 기존과 동일, Solana Explorer 링크 |
| EVM (sepolia) 계정에서 `pay account list` | `sepolia.etherscan.io` 링크 |
| `pay send --network sepolia` | "not yet supported on EVM" 에러 |
| `pay topup --network sepolia` | "not yet supported on EVM" 에러 |
| `pay send` (네트워크 미지정) | 기존과 동일 (mainnet 가정) |

---

## 다음 단계

[Phase 8: EVM 라이브 통합 테스트](./08-phase8-evm-integration-tests.md)
