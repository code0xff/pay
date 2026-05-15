# Phase 12: EVM 결제 UX 동등화 (P0+P1)

## 목표

Phase 7 에서 explorer 링크와 read-only 잔액 표시는 EVM 으로 확장됐지만,
**자금 이동 명령(`pay send`/`pay topup`)은 EVM 에서 hard-reject** 되어 있다.
그럼에도 `pay account new --chain-family evm` 직후 출력되는
`topup_required_body` 는 `pay topup` 을 안내한다. 사용자가 그 명령을 실행하면
"not yet supported on EVM" 에러 — **자체 모순 UX**.

본 Phase 는 다음 셋을 한 묶음으로 해결한다.

1. `pay account new --chain-family evm` 후처리 안내 EVM 분기 분리
2. `pay account import --chain-family evm` 후처리에 잔액 표시 추가 (Solana 와 동등)
3. `pay send` EVM 분기 구현 (ERC-20 `transfer` 트랜잭션 브로드캐스트)
4. `pay topup` EVM 분기 — Sepolia/Base-Sepolia faucet 링크 안내 + 메인넷은
   외부 지갑 안내로 단계적으로 (자체 명령으로 펀딩하지 않음)
5. facilitator 에러 메시지를 사람이 읽기 쉬운 매핑으로 변환

근거 파일:
- `rust/crates/cli/src/commands/send.rs:69, 125–134` (reject_evm_network)
- `rust/crates/cli/src/commands/topup.rs:17–22` (동일 거절 경로)
- `rust/crates/cli/src/commands/account/new.rs:457–474` (topup_required_body)
- `rust/crates/cli/src/commands/account/import.rs:184–303` (EVM 분기에 잔액 없음)
- `rust/crates/core/src/client/runner.rs::readable_verification_message`

---

## 원칙

1. **Solana send/topup 흐름은 코드 변경 없이 유지** — `reject_evm_network` 가
   제거되더라도 `SendCommand::run` 의 `network == MAINNET_NETWORK` 분기를 통해
   기존 경로로 빠진다.
2. **EVM `pay send` 는 ERC-20 직접 전송** — facilitator 의존 없음. CLI 사용자가
   소유한 키로 alloy provider 를 통해 `transferWithAuthorization` 이 아닌
   일반 `transfer(address,uint256)` 호출.
3. **`pay topup` 은 외부 도구로 위임** — 자체 fiat on-ramp 통합은 본 Phase 의
   스코프 외. CLI 는 단지 "올바른 곳으로 안내" 만 한다.
4. **메시지 일관성** — 새 EVM 후처리 메시지는 기존 Solana 메시지의 톤과 길이를
   따른다.

---

## 12-1. `account/new` 후처리 EVM 분기

### 현재 동작

`account/new.rs:457-474` `topup_required_body` 는 Solana 가정으로 작성:

```rust
fn topup_required_body(name: &str) -> String {
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        crate::commands::topup::topup_retry_command(name)
    )
}
```

`pay topup` 은 EVM 에서 거절되므로 사용자는 막다른 골목.

### 변경 설계

`print_account_summary` (또는 `new.rs` 후처리 함수) 에서 network 슬러그를
받아 EVM 이면 다른 안내문 출력:

```rust
fn topup_required_body(name: &str, network: &str, address: &str) -> String {
    if pay_core::accounts::is_evm_network_family(network) {
        return evm_funding_body(name, network, address);
    }
    format!(
        "A top-up is required before making paid requests.\n$ {}",
        crate::commands::topup::topup_retry_command(name)
    )
}

fn evm_funding_body(name: &str, network: &str, address: &str) -> String {
    let faucet_or_wallet = match network {
        "sepolia"      => "Faucet: https://sepolia-faucet.pk910.de\n         https://www.alchemy.com/faucets/ethereum-sepolia",
        "base-sepolia" => "Faucet: https://www.alchemy.com/faucets/base-sepolia",
        "holesky"      => "Faucet: https://holesky-faucet.pk910.de",
        // mainnet 체인: 외부 지갑/거래소
        _ => "Fund via MetaMask, Coinbase Wallet, or your preferred exchange.",
    };
    format!(
        "Fund the address before making paid requests.\n  Address: {address}\n  {faucet_or_wallet}\n\n  Then: pay account balance --network {network}"
    )
}
```

### 호출처 변경

- `account/new.rs::print_account_summary` 시그니처에 `network` 와 `address`
  전달 (이미 함수 내부에 있을 가능성 높음 — Read 로 정확한 시그니처 확인 후 패치).
- `account/import.rs::run_evm` 의 최종 출력에도 동일 헬퍼 호출.

---

## 12-2. `account/import` EVM 후처리에 잔액 표시

### 현재 동작

Solana import (`import.rs:84-89`) 는 `display_balance(&pubkey_b58)` 호출.
EVM 분기 (`run_evm`) 는 호출하지 않아 import 직후 사용자가 "잔액이 잡혔는지"
확인할 방법이 없음.

### 변경 설계

`run_evm` 마지막 단계 (accounts.save 직후, print_account_list 호출 전):

```rust
#[cfg(feature = "evm")]
let _ = display_evm_balance(network, &address).await;
```

`display_evm_balance` 는 `crate::components::account::print_evm_balance`
형태로 신규 구현하거나, 이미 `account list` 에서 사용 중인 path 를 재사용:

```rust
async fn display_evm_balance(network: &str, address: &str) {
    match pay_core::client::balance::get_evm_balances(network, address).await {
        Ok(balances) => crate::components::account::print_evm_balance_summary(&balances),
        Err(e) => eprintln!("  {} Balance lookup failed: {e}", "!".yellow()),
    }
}
```

import 흐름이 동기 함수이므로 `tokio::runtime::Builder` 로 작은 런타임
하나 만들거나, 기존 send 처럼 multi-thread runtime 헬퍼를 재사용한다.

---

## 12-3. `pay send` EVM 분기

### 새 분기 시그니처

```rust
impl SendCommand {
    pub fn run(self, ...) -> pay_core::Result<()> {
        // ... 기존 코드 ...
        if pay_core::accounts::is_evm_network_family(network) {
            #[cfg(feature = "evm")]
            { return self.run_evm(network, account_override, verbose); }
            #[cfg(not(feature = "evm"))]
            { return Err(missing_evm_feature_error()); }
        }
        // (이하 Solana 경로 그대로)
    }
}
```

`reject_evm_network` 헬퍼는 그대로 유지하되, `topup.rs` 외에는 호출하지 않도록
정리. `send.rs` 에서는 제거 (실제 분기로 대체되므로 불필요).

### `run_evm` 흐름

```rust
#[cfg(feature = "evm")]
async fn run_evm_impl(self, network: &str, account_override: Option<&str>, verbose: bool)
    -> pay_core::Result<SendResult>
{
    use alloy::providers::{ProviderBuilder, Provider};
    use alloy::primitives::{Address, U256};

    // 1. 키 로드 — 기존 keystore::load_evm_key + EvmChainSigner::from_bytes
    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => unreachable!(),
    };
    let store = FileAccountsStore::default();
    let (signer, _ephemeral) = pay_core::signer::load_evm_signer_for_network(
        network, &store, account_override, &intent_reason,
    )?;

    // 2. 수신자 파싱
    let to: Address = self.recipient.parse()
        .map_err(|e| Error::Config(format!("Invalid recipient address: {e}")))?;

    // 3. 통화 결정 — USDC 기본, --currency USDT 면 USDT
    let symbol = self.currency.as_deref().unwrap_or("USDC");
    let token = pay_core::client::balance::evm_stablecoin_address(network, symbol)
        .ok_or_else(|| Error::Config(format!("{symbol} not deployed on {network}")))?
        .parse::<Address>().unwrap();
    let decimals = stablecoin_decimals(symbol);  // 12-5 참조

    // 4. 금액 파싱
    let raw_amount = if self.amount == "max" {
        let bal = read_token_balance(&provider, token, signer.address()).await?;
        if bal.is_zero() { return Err(Error::Config(format!("No {symbol} to send"))); }
        bal
    } else {
        parse_token_amount(&self.amount, decimals)?  // 기존 헬퍼 재사용
    };

    // 5. RPC + provider 구성
    let rpc_url = pay_core::config::evm_rpc_url(network)?;  // 12-7 참조
    let wallet = alloy::network::EthereumWallet::from(signer.local_signer().clone());
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(rpc_url.parse().unwrap());

    // 6. ERC-20 transfer 트랜잭션 전송
    let call = transfer_call(token, to, raw_amount);  // sol! 매크로로 정의
    let pending = provider.send_transaction(call).await
        .map_err(|e| Error::Mpp(format!("eth_sendRawTransaction failed: {e}")))?;
    let receipt = pending.with_required_confirmations(1).get_receipt().await
        .map_err(|e| Error::Mpp(format!("Failed to wait for confirmation: {e}")))?;

    if !receipt.status() {
        return Err(Error::Mpp(format!("Transfer reverted: tx {}", receipt.transaction_hash)));
    }

    Ok(SendResult {
        signature: format!("{:?}", receipt.transaction_hash),
        currency: symbol.to_string(),
        amount_raw: raw_amount.to::<u128>(),
        total_amount_raw: raw_amount.to::<u128>(),
        decimals,
        fee_refund_raw: 0,
        from: format!("{:?}", signer.address()),
        to: format!("{:?}", to),
        network: network.to_string(),
        rpc_url,
    })
}
```

### 성공 메시지

기존 `send_success_title`/`send_success_body` 의 분기에 EVM 익스플로러 링크
추가. 이미 `components::account::evm_explorer_url` 가 있으므로
`evm_transaction_link(tx_hash, network)` 헬퍼만 추가.

### `--fee-within` 의미

EVM 에서는 gas 가 ETH 로 별도 차감되므로 stablecoin 금액에서 fee 를 깎지 않는다.
사용자가 `--fee-within` 을 전달해도 무시 + warning 출력:

```
warn: --fee-within is a no-op on EVM (gas is paid in native ETH, not USDC)
```

### `--memo` 의미

ERC-20 `transfer` 는 메모 필드가 없으므로 거절:

```rust
if self.memo.is_some() || self.memo_hex.is_some() {
    return Err(Error::Config(
        "--memo / --memo-hex are not supported on EVM networks. \
         For an on-chain note, send the transfer separately and \
         attach a sidecar message off-chain.".into()
    ));
}
```

---

## 12-4. `pay topup` EVM 분기

`pay topup` 은 본질적으로 **외부 자금을 받기 위한 안내 + monitoring** 흐름.
Solana 의 경우 TUI 가 잔액 변화를 polling 한다.

### 변경 설계

```rust
impl TopupCommand {
    pub fn run(self, network_override: Option<&str>) -> pay_core::Result<()> {
        let network = network_override.unwrap_or(MAINNET_NETWORK);
        if pay_core::accounts::is_evm_network_family(network) {
            return run_evm_topup(network, self.account.as_deref());
        }
        // (이하 Solana 경로 그대로)
    }
}

fn run_evm_topup(network: &str, account_override: Option<&str>) -> pay_core::Result<()> {
    let accounts = AccountsFile::load()?;
    let address = resolve_evm_address(&accounts, network, account_override)?;
    print_evm_funding_panel(network, &address);  // 12-1 의 evm_funding_body 재사용
    eprintln!("  {} Polling for incoming USDC...", "⟳".dimmed());

    // 60초간 잔액 polling — Solana TUI 와 동등한 UX
    let baseline = block_on(get_evm_usdc_balance(network, &address))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Config("No funds received within 60s. Try `pay account balance` later.".into()));
        }
        std::thread::sleep(Duration::from_secs(3));
        let now = block_on(get_evm_usdc_balance(network, &address))?;
        if now > baseline {
            print_topup_success_evm(network, &address, now - baseline);
            return Ok(());
        }
    }
}
```

복잡한 TUI 통합은 후속 트랙 — 본 Phase 는 단순 polling.

---

## 12-5. Stablecoin decimals 테이블

여러 곳에서 USDC=6 을 하드코딩 중이다 (`evm_x402_payment.rs:328`,
`balance.rs:520`). 일원화:

```rust
// rust/crates/types/src/stablecoin.rs (또는 client/balance.rs)
pub fn evm_stablecoin_decimals(symbol: &str) -> u32 {
    match symbol {
        "USDC" | "USDT" => 6,
        "DAI" => 18,
        _ => 6,  // 알려지지 않은 토큰은 보수적으로 6 (검증은 호출처에서)
    }
}
```

호출처:
- `evm_x402_payment.rs::build_evm_requirements`
- `client/evm.rs::build_evm_payment`
- `commands/send.rs::run_evm_impl`
- `client/balance.rs::get_evm_balances`

Phase 13 의 EIP-712 domain on-chain 조회와 묶어 처리해도 무방.

---

## 12-6. facilitator 에러 메시지 매핑

### 현재 동작

`runner.rs::collect_x402_failure` 가 facilitator 의 `invalidReason` 을 그대로
전달. 예: `"invalid signature for domain"`, `"insufficient funds"`. 사용자는
어떻게 고칠지 모른다.

### 변경 설계

`runner.rs` 에 `readable_evm_verification_message` 추가:

```rust
fn readable_evm_verification_message(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("insufficient funds") || lower.contains("transfer_amount_exceeds_balance") {
        return format!("{raw}\n  → Top up USDC on this EVM network before retrying.");
    }
    if lower.contains("invalid signature") || lower.contains("domain") {
        return format!("{raw}\n  → EIP-712 domain mismatch. The token contract may have a different (name,version). Report this with the network slug.");
    }
    if lower.contains("authorization_used") || lower.contains("nonce") {
        return format!("{raw}\n  → This payment authorization was already used. Reset by running the command again.");
    }
    raw.to_string()
}
```

`collect_x402_failure` 의 EVM 분기에서만 호출하도록 분기.

---

## 12-7. EVM RPC URL 출처

여러 명령이 EVM RPC URL 을 필요로 한다 (send, topup, 11-1 의 서버 receipt 검증).
한 곳에서 결정:

```rust
// rust/crates/core/src/config.rs
pub fn evm_rpc_url(network: &str) -> Result<String> {
    if let Ok(env) = std::env::var(format!("PAY_EVM_RPC_{}", network.to_uppercase().replace('-', "_"))) {
        return Ok(env);
    }
    let config = Config::load().unwrap_or_default();
    if let Some(url) = config.evm_rpc_urls.get(network) {
        return Ok(url.clone());
    }
    Ok(default_public_evm_rpc(network).to_string())
}

fn default_public_evm_rpc(network: &str) -> &'static str {
    match network {
        "ethereum"     => "https://eth.llamarpc.com",
        "base"         => "https://mainnet.base.org",
        "optimism"     => "https://mainnet.optimism.io",
        "arbitrum"     => "https://arb1.arbitrum.io/rpc",
        "sepolia"      => "https://ethereum-sepolia-rpc.publicnode.com",
        "holesky"      => "https://ethereum-holesky-rpc.publicnode.com",
        "base-sepolia" => "https://sepolia.base.org",
        _              => "https://eth.llamarpc.com",
    }
}
```

`Config` 에 `evm_rpc_urls: HashMap<String, String>` 추가 (옵셔널, 빈 맵 기본값).

> 공개 RPC 는 rate limit 가 있어 production 에서는 사용자가 자체 RPC 를 설정하도록
> README/CLAUDE.md 에 명시.

---

## 변경 파일 요약

| 파일 | 유형 | 변경 |
|------|------|------|
| `rust/crates/cli/src/commands/send.rs` | 수정 | EVM 분기 추가, `reject_evm_network` 호출 제거 |
| `rust/crates/cli/src/commands/topup.rs` | 수정 | EVM 분기 추가, polling 구현 |
| `rust/crates/cli/src/commands/account/new.rs` | 수정 | `topup_required_body` 에 network/address 전달, EVM 안내문 |
| `rust/crates/cli/src/commands/account/import.rs` | 수정 | `run_evm` 끝에 잔액 표시 |
| `rust/crates/core/src/client/send_evm.rs` | **신규** | `pay_core::client::send_evm::send_erc20` |
| `rust/crates/core/src/client/balance.rs` | 수정 | `evm_stablecoin_decimals` export |
| `rust/crates/core/src/config.rs` | 수정 | `evm_rpc_url(network)`, `Config::evm_rpc_urls` |
| `rust/crates/core/src/client/runner.rs` | 수정 | `readable_evm_verification_message` 추가 + EVM 분기 |
| `rust/crates/cli/src/components/account.rs` | 수정 | `evm_transaction_link(tx, network)` 헬퍼 |
| `rust/crates/types/src/stablecoin.rs` | 수정 (선택) | `evm_stablecoin_decimals` |

---

## 테스트 전략

### Unit

- `send_evm_rejects_memo` — `--memo` 와 함께 호출 시 명확한 에러
- `send_evm_rejects_unknown_symbol` — `--currency DOGE` → 에러
- `send_evm_max_uses_full_balance` — alloy mock provider 로 검증
- `topup_evm_funding_body_contains_faucet_for_sepolia`
- `topup_evm_funding_body_recommends_wallet_for_mainnet`
- `topup_required_body_after_account_new_evm_does_not_suggest_pay_topup`
- `readable_evm_verification_message_maps_insufficient_funds`

### 통합 (`evm,network_tests`)

- Sepolia 에 1 USDC 보내기 → 트랜잭션 해시 회수 → 익스플로러 링크 출력 검증
- `pay account import --chain-family evm` 직후 잔액 출력 캡처 → 0 USDC 라인
  포함 확인

---

## 비-목표

- fiat on-ramp (Stripe/Coinbase) EVM 통합 — 별도 트랙
- 멀티 토큰 라우팅 (USDC 보유, USDT 자동 변환 후 전송 등)
- gas estimation UI — alloy 기본 fee 알고리즘에 의존, 명시적 표시는 후속
