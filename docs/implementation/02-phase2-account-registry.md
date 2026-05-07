# Phase 2: EVM 계정 레지스트리 확장

## 목표

`accounts.yml`에 EVM 네트워크 항목을 추가할 수 있도록 `Account` 구조체를 확장한다.  
기존 Solana 항목은 그대로 동작해야 한다(하위 호환).

---

## 수정 파일: `rust/crates/core/src/accounts.rs`

### 2-1. `Account` 구조체 필드 추가

```rust
/// A single account entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// Which keystore backend stores the secret key.
    pub keystore: Keystore,

    #[serde(default, skip_serializing_if = "is_false")]
    pub active: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_required: Option<bool>,

    /// Base-58 pubkey (Solana) or 0x-hex address (EVM).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Base58-encoded full keypair (Solana ephemeral only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key_b58: Option<String>,

    // ── [신규] EVM 필드 ──────────────────────────────────────────────

    /// Chain family: "solana" | "evm". Defaults to "solana" when absent.
    /// Stored in YAML; controls key generation and signing path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_family: Option<String>,

    /// Hex-encoded 32-byte EVM private key (EVM ephemeral only, no 0x prefix).
    /// Analogous to `secret_key_b58` for Solana.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key_hex: Option<String>,

    // ── 기존 유지 ────────────────────────────────────────────────────

    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}
```

### 2-2. `Account` impl 메서드 추가

```rust
impl Account {
    // 기존 메서드 유지 ...

    /// Returns true if this account is EVM-family.
    pub fn is_evm(&self) -> bool {
        self.chain_family.as_deref() == Some("evm")
    }

    /// Returns the EVM private key bytes (32 bytes) for ephemeral EVM accounts.
    pub fn evm_key_bytes(&self) -> Option<Vec<u8>> {
        if !self.is_evm() {
            return None;
        }
        let hex = self.secret_key_hex.as_deref()?;
        hex::decode(hex.strip_prefix("0x").unwrap_or(hex)).ok()
    }
}
```

### 2-3. EVM 네트워크 헬퍼 함수 추가

```rust
/// Returns true for EVM networks that auto-generate ephemeral wallets.
pub fn is_evm_network(network: &str) -> bool {
    matches!(network, "sepolia" | "holesky" | "base-sepolia")
}

/// Returns true for any EVM network slug.
pub fn is_evm_network_family(network: &str) -> bool {
    matches!(network,
        "ethereum" | "base" | "optimism" | "arbitrum"
        | "sepolia" | "holesky" | "base-sepolia"
    )
}
```

### 2-4. `is_lazy_ephemeral_network` 확장

```rust
/// Solana + EVM testnets where missing-entry → auto-generate is safe.
fn is_lazy_ephemeral_network(network: &str) -> bool {
    matches!(network,
        // Solana (기존)
        "localnet" | "devnet"
        // EVM testnets (신규)
        | "sepolia" | "holesky" | "base-sepolia"
    )
}
```

### 2-5. `generate_ephemeral_account` 체인별 분기

```rust
/// Generate a fresh ephemeral account for the given network.
/// Chooses ed25519 (Solana) or secp256k1 (EVM) based on network slug.
fn generate_ephemeral_account_for_network(network: &str) -> Account {
    if is_evm_network_family(network) {
        generate_evm_ephemeral_account(network)
    } else {
        generate_solana_ephemeral_account()  // 기존 로직 함수로 추출
    }
}

fn generate_solana_ephemeral_account() -> Account {
    // 기존 generate_ephemeral_account() 내용 그대로
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();
    let mut full = Vec::with_capacity(64);
    full.extend_from_slice(&signing_key.to_bytes());
    full.extend_from_slice(&verifying_key.to_bytes());
    Account {
        keystore: Keystore::Ephemeral,
        active: false,
        auth_required: Some(false),
        pubkey: Some(bs58::encode(verifying_key.to_bytes()).into_string()),
        chain_family: None,      // 기본값 → Solana
        secret_key_b58: Some(bs58::encode(&full).into_string()),
        secret_key_hex: None,
        vault: None,
        account: None,
        path: None,
        created_at: Some(now_rfc3339()),
    }
}

fn generate_evm_ephemeral_account(network: &str) -> Account {
    use crate::chain::EvmChainSigner;
    use crate::chain::ChainFamily;

    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => 1, // fallback
    };
    let signer = EvmChainSigner::random(chain_id);
    Account {
        keystore: Keystore::Ephemeral,
        active: false,
        auth_required: Some(false),
        pubkey: Some(signer.address()),
        chain_family: Some("evm".to_string()),
        secret_key_b58: None,
        secret_key_hex: Some(signer.to_hex_key()),
        vault: None,
        account: None,
        path: None,
        created_at: Some(now_rfc3339()),
    }
}
```

### 2-6. `load_or_create_ephemeral_for_network_as` 호출 지점 수정

기존 `generate_ephemeral_account()` 호출을 `generate_ephemeral_account_for_network(network)` 으로 교체:

```rust
// Before:
let account = generate_ephemeral_account();

// After:
let account = generate_ephemeral_account_for_network(network);
```

---

## `signer.rs` 수정 — EVM signer 로더

`load_signer_for_network_with_intent`에 EVM 분기 추가:

```rust
pub fn load_signer_for_network_with_intent(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    intent: &AuthIntent,
) -> Result<(MemorySigner, Option<ResolvedEphemeral>)> {
    // 기존 Solana 로직 유지 ...
}

/// [신규] EVM ChainSigner 로더.
/// x402 멀티체인 경로에서만 사용. 기존 MPP/Session 경로는 기존 함수 유지.
pub fn load_evm_signer_for_network(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
) -> Result<(crate::chain::EvmChainSigner, Option<ResolvedEphemeral>)> {
    use crate::chain::{ChainFamily, EvmChainSigner};

    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => return Err(Error::Config(format!(
            "`{network}` is not an EVM network"
        ))),
    };

    let file = store.load()?;
    let account_name = account_override.unwrap_or("default");

    // 기존 계정 조회
    if let Some(account) = file.named_account_for_network(network, account_name).cloned() {
        if !account.is_evm() {
            return Err(Error::Config(format!(
                "Account `{account_name}` on `{network}` is not EVM-family"
            )));
        }
        let key_hex = account.secret_key_hex.as_deref().ok_or_else(|| {
            Error::Config("EVM ephemeral account missing secret_key_hex".to_string())
        })?;
        let signer = EvmChainSigner::from_hex(key_hex, chain_id)?;
        return Ok((signer, None));
    }

    // Lazy ephemeral 생성 (테스트넷만)
    if is_lazy_ephemeral_network(network) {
        let resolved = load_or_create_ephemeral_for_network_as(network, account_name, store)?;
        let key_hex = resolved.account.secret_key_hex.as_deref().ok_or_else(|| {
            Error::Config("Generated EVM ephemeral missing secret_key_hex".to_string())
        })?;
        let signer = EvmChainSigner::from_hex(key_hex, chain_id)?;
        return Ok((signer, Some(resolved)));
    }

    Err(Error::Config(format!(
        "No EVM account configured for network `{network}`.\n\
         Run `pay account new --network {network}` to create one."
    )))
}
```

---

## YAML 예시

```yaml
version: 2
accounts:
  # 기존 Solana — 변경 없음
  mainnet:
    default:
      keystore: apple-keychain
      auth_required: true
      pubkey: "7xKX...abc"

  # [신규] Ethereum mainnet
  ethereum:
    default:
      keystore: apple-keychain
      chain_family: evm
      auth_required: true
      pubkey: "0x1234...abcd"

  # [신규] Base mainnet
  base:
    default:
      keystore: apple-keychain
      chain_family: evm
      auth_required: true
      pubkey: "0x1234...abcd"

  # [신규] Sepolia testnet — ephemeral auto-generated
  sepolia:
    default:
      keystore: ephemeral
      chain_family: evm
      auth_required: false
      pubkey: "0xabcd...1234"
      secret_key_hex: "deadbeef..."
      created_at: "2026-05-07T00:00:00Z"
```

---

## 검증

```bash
cargo test -p pay-core accounts

# 예상 통과 테스트
# accounts::tests::evm_ephemeral_generated_for_sepolia
# accounts::tests::evm_account_is_evm_true
# accounts::tests::solana_account_is_evm_false
# accounts::tests::yaml_skips_evm_fields_when_absent
# accounts::tests::evm_key_bytes_roundtrip
```

---

## 다음 단계

Phase 3: [x402 멀티체인 파싱·서명](./03-phase3-x402-multichain.md)
