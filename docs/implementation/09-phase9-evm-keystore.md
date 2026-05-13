# Phase 9: EVM 키스토어 백엔드

## 목표

OS 키스토어(Apple Keychain, GNOME Keyring, Windows Credential Manager,
1Password)에 secp256k1(EVM) 개인키를 저장·로드할 수 있도록 한다.

현재 `signer.rs::load_evm_signer_for_network`는 다음 에러를 반환한다:

```text
EVM account `default` on `sepolia` is missing `secret_key_hex`.
Keystore-backed EVM accounts are not yet supported.
```

이 메시지는 Phase 2에서 의도적으로 남긴 placeholder다. Phase 9에서 제거한다.

**원칙**

1. ed25519 (Solana) 보관 동작은 절대 변경하지 않는다.
2. EVM 분기는 키 길이(32B vs 64B)로 명확히 구분한다.
3. 키스토어 백엔드 추상화(`SecretStore`)는 바이트 슬라이스 그대로 다루므로 수정 불필요.

---

## 9-1. 키 보관 포맷

### 현재 (Solana)

`crates/keystore/src/store.rs`:

```rust
const KEYPAIR_LEN: usize = 64;             // ed25519 keypair: 32B priv + 32B pub
const KEYPAIR_KEY_PREFIX: &str = "keypair:";
```

키스토어는 `keypair:<account-name>` 키에 64바이트 페이로드를 저장.

### 변경 설계

EVM은 32바이트 secp256k1 비밀키만 보관한다(공개키는 비밀키로부터 도출 가능).
계정 이름 충돌을 피하기 위해 별도 prefix를 사용.

```rust
const ED25519_KEYPAIR_LEN: usize = 64;          // 기존 KEYPAIR_LEN 리네임
const SECP256K1_PRIVKEY_LEN: usize = 32;
const ED25519_PREFIX: &str = "keypair:";        // 기존 KEYPAIR_KEY_PREFIX 리네임
const SECP256K1_PREFIX: &str = "evm-key:";

fn ed25519_key(account: &str) -> String { format!("{ED25519_PREFIX}{account}") }
fn secp256k1_key(account: &str) -> String { format!("{SECP256K1_PREFIX}{account}") }
```

### 하위 호환

기존 `keypair:` prefix 키들은 그대로 두고 ed25519 전용 함수에서만 참조한다.
이미 저장된 사용자 데이터에는 영향이 없다.

---

## 9-2. 키스토어 API 확장

`crates/keystore/src/store.rs`의 공개 메서드에 EVM 변형을 추가한다. 기존
시그니처는 ed25519 의미를 유지하고, 새 메서드는 `_evm_` 접두사로 구분한다.

### import

```rust
impl Keystore {
    /// Existing — stores a 64-byte ed25519 keypair.
    pub fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        if keypair_bytes.len() != ED25519_KEYPAIR_LEN {
            return Err(Error::InvalidKey(format!(
                "expected {ED25519_KEYPAIR_LEN} bytes, got {}",
                keypair_bytes.len()
            )));
        }
        let key = ed25519_key(account);
        self.store.set(&key, keypair_bytes)
    }

    /// New — stores a 32-byte secp256k1 private key.
    pub fn import_evm_key(&self, account: &str, privkey_bytes: &[u8]) -> Result<()> {
        if privkey_bytes.len() != SECP256K1_PRIVKEY_LEN {
            return Err(Error::InvalidKey(format!(
                "expected {SECP256K1_PRIVKEY_LEN} bytes for secp256k1 privkey, got {}",
                privkey_bytes.len()
            )));
        }
        let key = secp256k1_key(account);
        self.store.set(&key, privkey_bytes)
    }
}
```

### load

```rust
impl Keystore {
    pub fn load_evm_key(&self, account: &str, reason: &str) -> Result<Zeroizing<Vec<u8>>> {
        self.load_evm_key_with_intent(account, &AuthIntent::from_reason(reason))
    }

    pub fn load_evm_key_with_intent(
        &self,
        account: &str,
        intent: &AuthIntent,
    ) -> Result<Zeroizing<Vec<u8>>> {
        if self.auth_required {
            self.auth.authenticate_intent(intent)
                .map_err(|_| Error::AuthDenied("user cancelled".to_string()))?;
        }
        let bytes = self.store.get(&secp256k1_key(account))?;
        if bytes.len() != SECP256K1_PRIVKEY_LEN {
            return Err(Error::InvalidKey(format!(
                "secp256k1 privkey for `{account}` is {} bytes (expected {SECP256K1_PRIVKEY_LEN})",
                bytes.len()
            )));
        }
        Ok(Zeroizing::new(bytes))
    }

    pub fn delete_evm_key(&self, account: &str, reason: &str) -> Result<()> {
        // intent 처리는 기존 delete와 동일하게.
        self.store.delete(&secp256k1_key(account))
    }

    pub fn evm_key_exists(&self, account: &str) -> bool {
        self.store.get(&secp256k1_key(account)).is_ok()
    }
}
```

### 단위 테스트

`crates/keystore/src/store.rs::tests`에 추가:

```rust
#[test]
fn evm_key_roundtrip() {
    let ks = Keystore::in_memory();
    let priv_bytes = [42u8; 32];
    ks.import_evm_key("evm-alice", &priv_bytes).unwrap();
    let loaded = ks.load_evm_key("evm-alice", "test").unwrap();
    assert_eq!(&*loaded, &priv_bytes[..]);
}

#[test]
fn evm_key_rejects_wrong_length() {
    let ks = Keystore::in_memory();
    let too_short = [1u8; 16];
    let err = ks.import_evm_key("a", &too_short).unwrap_err();
    assert!(err.to_string().contains("expected 32 bytes"));
}

#[test]
fn ed25519_and_evm_keys_coexist_under_same_account_name() {
    // 같은 account 문자열이지만 prefix가 달라 충돌 없음.
    let ks = Keystore::in_memory();
    let ed = [1u8; 64];
    let evm = [2u8; 32];
    ks.import("alice", &ed, SyncMode::Local).unwrap();
    ks.import_evm_key("alice", &evm).unwrap();
    assert_eq!(&*ks.load_keypair("alice", "test").unwrap(), &ed[..]);
    assert_eq!(&*ks.load_evm_key("alice", "test").unwrap(), &evm[..]);
}
```

---

## 9-3. accounts.rs / signer.rs 분기

### accounts.yml 표현

```yaml
ethereum:
  default:
    keystore: apple-keychain    # OS 키스토어 백엔드
    chain_family: evm
    pubkey: "0x1234...abcd"     # EIP-55 hex (서명 시 검증용)
    # secret_key_hex는 없음 — 키스토어에서 로드.
```

기존 ephemeral EVM은 `secret_key_hex` 인라인을 유지(변경 없음).

### `Account` 헬퍼

`crates/core/src/accounts.rs`:

```rust
impl Account {
    pub fn is_evm(&self) -> bool {
        self.chain_family.as_deref() == Some("evm")
    }

    /// True when the account is EVM-family AND backed by a hardware/OS keystore
    /// (not inline `secret_key_hex`).
    pub fn is_evm_keystore(&self) -> bool {
        self.is_evm() && !matches!(self.keystore, Keystore::Ephemeral)
    }
}
```

### `signer.rs::load_evm_signer_for_network`

현재 ephemeral 경로만 처리한다. 키스토어 분기를 추가.

```rust
#[cfg(feature = "evm")]
pub fn load_evm_signer_for_network(
    network: &str,
    store: &dyn crate::accounts::AccountsStore,
    account_override: Option<&str>,
) -> Result<(EvmChainSigner, Option<ResolvedEphemeral>)> {
    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => return Err(Error::Config(format!("Network `{network}` is not EVM"))),
    };

    let file = store.load()?;
    let account_name = account_override.unwrap_or(DEFAULT_ACCOUNT_NAME);

    if let Some(account) = file.named_account_for_network(network, account_name).cloned() {
        if !account.is_evm() {
            return Err(Error::Config(format!(
                "Account `{account_name}` on `{network}` is not EVM-family"
            )));
        }

        // (A) Ephemeral — inline hex (Phase 2의 기존 경로).
        if matches!(account.keystore, Keystore::Ephemeral) {
            let key_hex = account.secret_key_hex.as_deref().ok_or_else(|| {
                Error::Config("Ephemeral EVM account missing `secret_key_hex`".to_string())
            })?;
            let signer = EvmChainSigner::from_hex(key_hex, chain_id)?;
            return Ok((signer, None));
        }

        // (B) Keystore-backed — Phase 9 신규.
        let intent = crate::keystore::AuthIntent::default_payment()
            .with_account_context(account_name);
        let backend = keystore_backend_for_account(&account)?;
        let bytes = backend
            .load_evm_key_with_intent(account_name, &intent)
            .map_err(|e| map_keystore_backend_error("evm", e))?;
        let hex_key = hex::encode(&*bytes);
        let signer = EvmChainSigner::from_hex(&hex_key, chain_id)?;
        return Ok((signer, None));
    }

    // (C) Lazy ephemeral on testnets (기존 경로).
    if is_evm_lazy_network(network) {
        let resolved = load_or_create_ephemeral_for_network_as(network, account_name, store)?;
        let key_hex = resolved.account.secret_key_hex.as_deref().ok_or_else(|| {
            Error::Config("Generated EVM ephemeral is missing `secret_key_hex`".to_string())
        })?;
        let signer = EvmChainSigner::from_hex(key_hex, chain_id)?;
        return Ok((signer, Some(resolved)));
    }

    Err(Error::Config(format!(
        "No EVM account configured for network `{network}`."
    )))
}

fn keystore_backend_for_account(account: &Account) -> Result<crate::keystore::Keystore> {
    Ok(match account.keystore {
        Keystore::AppleKeychain  => crate::keystore::Keystore::apple_keychain(),
        Keystore::GnomeKeyring   => crate::keystore::Keystore::gnome_keyring(),
        Keystore::WindowsHello   => crate::keystore::Keystore::windows_hello(),
        Keystore::OnePassword    => crate::keystore::Keystore::onepassword(account.account.clone()),
        Keystore::Ephemeral      => unreachable!("handled above"),
        Keystore::File           => {
            return Err(Error::Config(
                "File-backed EVM accounts are not supported".to_string(),
            ));
        }
    })
}
```

---

## 9-4. CLI 흐름

### `pay account new --chain-family evm --network sepolia`

```rust
// crates/cli/src/commands/account/new.rs (요지)
match chain_family.as_deref() {
    Some("evm") => {
        let signer = pay_core::chain::EvmChainSigner::random(chain_id_for(network));
        let priv_bytes = hex::decode(signer.to_hex_key()).expect("32B hex");
        if backend != Keystore::Ephemeral {
            pay_core::keystore::Keystore::from_backend(backend)
                .import_evm_key(account_name, &priv_bytes)?;
            // Account.pubkey만 저장하고 secret_key_hex는 비움.
        } else {
            // Phase 2의 기존 동작 — inline hex.
        }
    }
    _ => {
        // Solana 기존 경로 (변경 없음).
    }
}
```

### `pay account import --chain-family evm`

```text
$ pay account import --chain-family evm --network sepolia --name alice
Enter secp256k1 hex private key (with or without 0x prefix):
0x...

# 검증:
# 1. 0x prefix 제거 후 hex decode → 정확히 32바이트?
# 2. EvmChainSigner::from_hex로 도출되는 주소를 화면에 표시 (사용자 확인).
# 3. 키스토어에 import_evm_key 호출.
```

### `pay account destroy --name alice --network sepolia`

```rust
// 기존 destroy 흐름 + EVM 분기:
if account.is_evm_keystore() {
    backend.delete_evm_key(account_name, "delete EVM account")?;
}
// accounts.yml에서 항목 삭제 — 기존과 동일.
```

---

## 9-5. 마이그레이션

기존 `secret_key_hex` 인라인 ephemeral 계정을 키스토어로 옮기는 헬퍼:

```bash
$ pay account migrate-to-keystore --name alice --network sepolia
✓ moved secret_key_hex from accounts.yml into Apple Keychain
✓ accounts.yml.keystore: ephemeral → apple-keychain
```

설계는 Phase 9 1차 범위 밖. 추후 확장.

---

## 9-6. 검증

### 단위 테스트

```bash
cargo test -p pay-keystore evm_key_roundtrip
cargo test -p pay-keystore evm_key_rejects_wrong_length
cargo test -p pay-keystore ed25519_and_evm_keys_coexist_under_same_account_name
cargo test -p pay-core --features evm signer::tests::load_evm_signer_via_keystore
```

### 수동 검증 (macOS)

```bash
# 1. 키스토어 백엔드로 새 EVM 계정 생성
pay account new --chain-family evm --network sepolia --keystore keychain --name alice
# Touch ID 프롬프트 → 등록 → accounts.yml에 항목 추가, Keychain에 evm-key:alice 저장.

# 2. 잔액 조회 (서명 없이 조회만)
pay --network sepolia account list
# alice의 잔액이 표시되는지 확인 (Touch ID 안 뜨면 OK).

# 3. 실제 결제 시 Touch ID 요청
pay --network sepolia curl https://api.example.com/x402-endpoint
# Touch ID 프롬프트 → 승인 → 결제 진행.

# 4. 삭제
pay account destroy --name alice --network sepolia
# Keychain에서 evm-key:alice 제거되는지 확인.
```

---

## 9-7. 보안 검토

- secp256k1 32바이트 평문이 OS 키스토어에 저장된다. ed25519와 동일한 보안
  모델(생체 인증 + 하드웨어 enclave). 추가 위협 없음.
- `Zeroizing<Vec<u8>>`로 메모리 해제 시 zeroing. 기존 패턴 그대로 따른다.
- `EvmChainSigner::from_hex`는 alloy `PrivateKeySigner`가 내부에서 zeroize함.
- `hex::encode(&*bytes)`는 임시 String을 만든다 — `Zeroizing<String>`으로 감싸는
  것이 이상적이지만 alloy `parse::<PrivateKeySigner>()`가 `&str`을 요구하므로
  마이크로 윈도우는 존재. 차후 alloy가 `from_slice`를 지원하면 평문 hex 단계를
  제거.

---

## 다음 단계

- Phase 9가 마지막 후속 작업. 완료 후 EVM 멀티체인 트랙은 production-ready로
  간주.
- Phase 6 (x402 서버 프록시)와는 무관 — 별도 트랙에서 진행.
