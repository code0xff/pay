# Phase 10: EVM 키스토어 CLI 진입점

## 개요

Phase 9에서 라이브러리 MVP로 도입된 secp256k1 OS 키스토어 백엔드
(`import_evm_key`, `load_evm_key`, `delete_evm_key`, `evm_key_exists`)를
CLI 명령으로 노출한다. 사용자는 `--chain-family evm` 플래그 하나로
Solana CLI와 대칭되는 흐름을 사용할 수 있다.

본 문서는 회고형 설계 문서로, 구현은 다음 커밋들로 이미 머지되었다.

- `89c1ce7` — `pay account new --chain-family evm`
- `89c1ce7` — `pay account destroy` EVM 분기
- `5aa9912` — `pay account import --chain-family evm --secret-key-hex`

---

## 원칙

1. **단일 진입점, 분기 최소화** — `--chain-family evm` 가 명시되면 `run_evm()`
   메서드로 위임하고, 그 외 모든 케이스는 기존 Solana 경로를 변경 없이 사용한다.
2. **feature 게이팅 명확** — `evm` feature off 빌드에서 `--chain-family evm`
   사용 시 `"EVM ... require the `evm` Cargo feature."` 명시 에러로 반환.
3. **공통 헬퍼는 `account/new.rs::create_evm_account()`** — `new`/`import` 둘 다
   keystore 생성, 충돌 확인, accounts.yml upsert 흐름을 공유한다.
4. **사용자 확인을 derivation 전에 표시** — `import` 시 `address` 를 먼저 출력해서
   사용자가 잘못된 키를 paste 했을 때 keystore 봉인 전 취소할 수 있게 한다.

---

## CLI 표면

### `pay account new --chain-family evm --network <slug> [--backend <id>]`

- secp256k1 키페어 신규 생성
- OS 키스토어에 `evm-key:<name>` 항목 봉인
- `accounts.yml` 에 `chain_family: evm`, `pubkey: 0xEIP55Address` 저장
- 출력: 주소 + 익스플로러 링크 + 펀딩 안내

### `pay account import --chain-family evm --network <slug> --secret-key-hex 0x...`

- 32-byte hex 비밀키 검증 (`EvmChainSigner::from_hex`, curve order 체크 포함)
- derived 주소를 콘솔에 먼저 출력
- 동일 주소가 이미 등록되어 있으면 `Confirm` 으로 확인
- keystore 봉인 → `accounts.yml` upsert

### `pay account destroy --network <slug>` (EVM 자동 감지)

- `Account::is_evm()` 가 true 면 `evm-key:` 분기 사용
- `delete_evm_key_with_intent()` 호출 후 accounts.yml 에서 항목 제거
- Solana 경로(`delete_key`) 는 그대로 유지

---

## 변경 파일

| 파일 | 유형 | 핵심 변경 |
|------|------|---------|
| `rust/crates/cli/src/commands/account/new.rs` | 수정 | `--chain-family` 플래그, `run_evm()`, `create_evm_account()` (공용 헬퍼) |
| `rust/crates/cli/src/commands/account/import.rs` | 수정 | `--chain-family`, `--network`, `--secret-key-hex` 플래그, `run_evm()` |
| `rust/crates/cli/src/commands/account/destroy.rs` | 수정 | `Account::is_evm()` 분기 추가, `delete_evm_key_with_intent` 호출 |

---

## 구현 상세

### Step 1 — 공용 EVM 생성 헬퍼

`account/new.rs::create_evm_account()` 시그니처:

```rust
#[cfg(feature = "evm")]
pub fn create_evm_account(
    name: &str,
    network: &str,
    backend: Option<&str>,
    vault: Option<&str>,
    force: bool,
) -> pay_core::Result<(String /* eip55 address */, &'static str /* backend display */)>;
```

`account/new.rs` 와 `account/import.rs` 둘 다 이 헬퍼를 호출하지만,
`import` 는 키를 새로 생성하지 않고 사용자가 paste 한 키를 직접 봉인하므로
같은 헬퍼를 **재사용할 수 없다**. 대신 import 는 `EvmChainSigner::from_hex` 로
검증 후 동일한 `import_evm_key_with_intent` API 를 호출하여 store-side
경로를 통일한다.

### Step 2 — `--chain-family evm` 분기 패턴

각 명령은 다음 패턴으로 진입한다:

```rust
if self.chain_family.as_deref() == Some("evm") {
    #[cfg(feature = "evm")]
    { return self.run_evm(); }
    #[cfg(not(feature = "evm"))]
    {
        return Err(pay_core::Error::Config(
            "EVM ... require the `evm` Cargo feature. Rebuild with \
             `cargo build -p pay --features evm`.".to_string(),
        ));
    }
}
// (이하 Solana 경로)
```

### Step 3 — `destroy` 의 EVM 분기

`account/destroy.rs:138` 부근:

```rust
let entry = accounts.account_for_network(&network).ok_or(...)?;
let is_evm = entry.is_evm();
let intent = pay_core::keystore::AuthIntent::destroy_account(&self.account);
if is_evm {
    ks.delete_evm_key_with_intent(&self.account, &intent)?;
} else {
    ks.delete_key_with_intent(&self.account, &intent)?;
}
```

### Step 4 — `import` 의 EVM 분기

`account/import.rs:184` 부근:

```rust
fn run_evm(self) -> pay_core::Result<()> {
    // 1. 네트워크 슬러그 검증 (is_evm_network_family)
    // 2. EvmChainSigner::from_hex 로 키 파싱 (chain_id 와 함께)
    // 3. eprintln!("Address: {}", signer.address())
    // 4. 동일 pubkey 가 이미 다른 네트워크/이름에 등록되어 있으면 Confirm
    // 5. backend 선택 + 키 봉인
    // 6. accounts.yml upsert
    // 7. account list 재출력 (highlight: new entry)
}
```

---

## 테스트 전략

| 테스트 | 위치 | 검증 |
|-------|------|------|
| `account_new_evm_writes_eip55_pubkey` | `new.rs::tests` | accounts.yml 에 0x...EIP55 주소 저장 |
| `account_import_evm_rejects_invalid_hex` | `import.rs::tests` | curve order/길이 위반 시 명확한 에러 |
| `account_import_evm_shows_address_before_save` | manual / integration | 주소가 출력에 포함되는지 |
| `account_destroy_evm_removes_keystore_entry` | `destroy.rs::tests` | `evm_key_exists()` → false |
| feature off 빌드에서 `--chain-family evm` | CI | "require the `evm` Cargo feature" 에러 |

---

## 비고

- `pay account export --chain-family evm` 는 **의도적으로 미구현**. Phase 9 보안
  결정과 일치 — EVM 비밀키 평문 export 는 사용자 노출 비용이 크고,
  대안(메타마스크 import flow 가이드) 이 더 안전하다.
- `--backend 1password` 는 EVM 에서도 동일하게 동작해야 하지만, 1Password Connect
  통합 자체가 라이브러리 외부에 있어 검증은 Solana 와 동일한 수동 절차로 미룬다.
