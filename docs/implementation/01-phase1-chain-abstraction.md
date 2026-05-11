# Phase 1: Chain 추상화 레이어

## 목표

ed25519(Solana)와 secp256k1(EVM) 서명을 동일한 인터페이스로 다룰 수 있는 `ChainSigner` 트레잇을 정의한다.  
기존 `MemorySigner` 코드는 건드리지 않고 래퍼로 감싼다.

---

## 1.0 Cargo feature flag (precondition)

Phase 1을 시작하기 전에 **`evm` feature가 `pay-core`에 정의되어 있어야 한다** (Gate G2에서 처리). 이는 Phase 1~5 전체의 사전조건이며, EVM 의존성은 `evm` feature가 켜질 때만 빌드 그래프에 포함된다.

**Gate G2에서 정비된 상태:**
- `rust/Cargo.toml`의 `[workspace.dependencies]`에 `alloy`, `x402-chain-eip155`, `hex` 항목이 존재한다.
- `rust/crates/core/Cargo.toml`의 `[features]`에 `evm = ["dep:alloy", "dep:x402-chain-eip155", "dep:hex"]`가 존재하고, 세 의존성은 `optional = true`로 선언되어 있다.
- `rust/crates/cli/Cargo.toml`의 `[features]`에 `evm = ["pay-core/evm"]`가 존재한다.

**Phase 1에서 추가로 처리할 gating:**
- `chain.rs` 자체는 내부에 `#[cfg]` 표기 없이 무조건적으로 작성한다(모듈 자체가 gated되므로).
- `rust/crates/core/src/lib.rs`에서 `pub mod chain;` 선언은 **모듈 선언 라인에 feature gate를 붙인다**:
  ```rust
  #[cfg(feature = "evm")]
  pub mod chain;
  ```

**검증** (Phase 1 작성 후):
```bash
cd rust

# alloy가 기본 빌드 dep 트리에 들어가지 않는지 확인
cargo tree -p pay-core | grep -c alloy            # → 0

# 기본 빌드는 chain 모듈 없이 통과
cargo build

# EVM 활성 시 chain 모듈이 컴파일됨
cargo build --features evm
cargo test -p pay-core --features evm chain
```

---

## 수정 파일

### 1. `rust/Cargo.toml` — 의존성 추가

```toml
[workspace.dependencies]
# 기존 유지 ...

# [신규] EVM 지원 (G2에서 정비됨, optional은 pay-core에서 처리)
alloy = { version = "1.7.3", features = [
    "signer-local",
    "provider-http",
    "eip712",
    "sol-types",
    "rpc-types",
] }
x402-chain-eip155 = "1.4.4"
hex = "0.4"
```

### 2. `rust/crates/core/Cargo.toml` — 크레이트 의존성 추가 (optional)

```toml
[features]
# 기존 유지 ...
evm = ["dep:alloy", "dep:x402-chain-eip155", "dep:hex"]

[dependencies]
# 기존 유지 ...

# [신규] — optional = true 로 선언, `evm` feature가 활성화되면 자동으로 빌드 그래프에 추가됨
alloy             = { workspace = true, optional = true }
x402-chain-eip155 = { workspace = true, optional = true }
hex               = { workspace = true, optional = true }
```

---

## 신규 파일: `rust/crates/core/src/chain.rs`

```rust
//! Chain family abstraction — wraps ed25519 (Solana) and secp256k1 (EVM).
//!
//! New protocols plug in by implementing `ChainSigner`.
//! Existing Solana code continues to use `MemorySigner` directly; this
//! module is only used by the x402 multi-chain dispatch path.

use crate::{Error, Result};
// `with_chain_id` lives on alloy's `Signer` trait, not the PrivateKeySigner struct.
use alloy::signers::Signer;

// ── ChainFamily ──────────────────────────────────────────────────────────────

/// Identifies the on-chain ecosystem for a given account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainFamily {
    Solana,
    Evm { chain_id: u64 },
}

impl ChainFamily {
    /// Parse a network slug (accounts.yml key) into a ChainFamily.
    pub fn from_network_slug(slug: &str) -> Self {
        match slug {
            "mainnet" | "devnet" | "testnet" | "localnet" => ChainFamily::Solana,
            "ethereum"    => ChainFamily::Evm { chain_id: 1 },
            "base"        => ChainFamily::Evm { chain_id: 8453 },
            "optimism"    => ChainFamily::Evm { chain_id: 10 },
            "arbitrum"    => ChainFamily::Evm { chain_id: 42161 },
            "sepolia"     => ChainFamily::Evm { chain_id: 11155111 },
            "holesky"     => ChainFamily::Evm { chain_id: 17000 },
            "base-sepolia"=> ChainFamily::Evm { chain_id: 84532 },
            other => {
                // "eip155:1" 형식 직접 처리
                if let Some(id_str) = other.strip_prefix("eip155:") {
                    if let Ok(id) = id_str.parse::<u64>() {
                        return ChainFamily::Evm { chain_id: id };
                    }
                }
                ChainFamily::Solana
            }
        }
    }

    /// Parse a CAIP-2 chain identifier into a ChainFamily.
    pub fn from_caip2(caip2: &str) -> Option<Self> {
        if caip2.starts_with("solana:") {
            return Some(ChainFamily::Solana);
        }
        if let Some(id_str) = caip2.strip_prefix("eip155:") {
            let chain_id = id_str.parse::<u64>().ok()?;
            return Some(ChainFamily::Evm { chain_id });
        }
        None
    }

    /// Convert to a pay-internal network slug.
    pub fn to_network_slug(&self) -> &'static str {
        match self {
            ChainFamily::Solana => "mainnet",
            ChainFamily::Evm { chain_id: 1 }        => "ethereum",
            ChainFamily::Evm { chain_id: 8453 }      => "base",
            ChainFamily::Evm { chain_id: 10 }        => "optimism",
            ChainFamily::Evm { chain_id: 42161 }     => "arbitrum",
            ChainFamily::Evm { chain_id: 11155111 }  => "sepolia",
            ChainFamily::Evm { chain_id: 17000 }     => "holesky",
            ChainFamily::Evm { chain_id: 84532 }     => "base-sepolia",
            ChainFamily::Evm { .. }                  => "evm-unknown",
        }
    }

    pub fn is_evm(&self) -> bool {
        matches!(self, ChainFamily::Evm { .. })
    }

    pub fn is_solana(&self) -> bool {
        matches!(self, ChainFamily::Solana)
    }
}

// ── ChainSigner trait ────────────────────────────────────────────────────────

/// Unified signing interface used by the x402 multi-chain dispatch path.
///
/// Today this is implemented only for EVM (`EvmChainSigner`). The Solana
/// payment path in x402 reuses the existing `solana-x402` SDK directly,
/// so wrapping `MemorySigner` here would be unused indirection. If a
/// Solana implementation is needed later, add it then — `solana_keychain::
/// SolanaSigner` is async-only so the trait method here would need to
/// become `async fn` (or block on a runtime).
pub trait ChainSigner: Send + Sync {
    /// Sign a raw message (already hashed if needed by the scheme).
    fn sign_raw(&self, message: &[u8]) -> Vec<u8>;

    /// Public address — 0x-hex for EVM.
    fn address(&self) -> String;

    /// Which chain this signer operates on.
    fn chain_family(&self) -> ChainFamily;
}

// ── EvmChainSigner ───────────────────────────────────────────────────────────

/// Wraps alloy `PrivateKeySigner` behind `ChainSigner`.
pub struct EvmChainSigner {
    pub signer: alloy::signers::local::PrivateKeySigner,
    pub chain_id: u64,
}

impl EvmChainSigner {
    /// Create from a 32-byte hex private key (with or without "0x" prefix).
    pub fn from_hex(hex_key: &str, chain_id: u64) -> Result<Self> {
        let key = hex_key.strip_prefix("0x").unwrap_or(hex_key);
        let signer = key.parse::<alloy::signers::local::PrivateKeySigner>()
            .map_err(|e| Error::Config(format!("Invalid EVM private key: {e}")))?;
        let signer = signer.with_chain_id(Some(chain_id));
        Ok(Self { signer, chain_id })
    }

    /// Generate a fresh random ephemeral signer.
    pub fn random(chain_id: u64) -> Self {
        let signer = alloy::signers::local::PrivateKeySigner::random()
            .with_chain_id(Some(chain_id));
        Self { signer, chain_id }
    }

    /// Export 32-byte private key as lowercase hex (no 0x prefix).
    pub fn to_hex_key(&self) -> String {
        hex::encode(self.signer.credential().to_bytes())
    }
}

impl ChainSigner for EvmChainSigner {
    fn sign_raw(&self, message: &[u8]) -> Vec<u8> {
        // Synchronous signing via alloy's blocking interface.
        // For EIP-712 signed payloads, the caller is responsible for
        // pre-hashing the message before passing it here.
        use alloy::signers::SignerSync;
        self.signer
            .sign_message_sync(message)
            .map(|sig| sig.as_bytes().to_vec())
            .unwrap_or_default()
    }

    fn address(&self) -> String {
        format!("{:?}", self.signer.address())
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm { chain_id: self.chain_id }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_family_from_slug() {
        assert_eq!(ChainFamily::from_network_slug("mainnet"), ChainFamily::Solana);
        assert_eq!(ChainFamily::from_network_slug("ethereum"), ChainFamily::Evm { chain_id: 1 });
        assert_eq!(ChainFamily::from_network_slug("base"), ChainFamily::Evm { chain_id: 8453 });
        assert_eq!(ChainFamily::from_network_slug("sepolia"), ChainFamily::Evm { chain_id: 11155111 });
    }

    #[test]
    fn chain_family_from_caip2() {
        assert_eq!(ChainFamily::from_caip2("eip155:1"), Some(ChainFamily::Evm { chain_id: 1 }));
        assert_eq!(ChainFamily::from_caip2("eip155:8453"), Some(ChainFamily::Evm { chain_id: 8453 }));
        assert!(ChainFamily::from_caip2("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp")
            .unwrap().is_solana());
        assert_eq!(ChainFamily::from_caip2("unknown:123"), None);
    }

    #[test]
    fn evm_signer_random_has_valid_address() {
        let signer = EvmChainSigner::random(1);
        let addr = signer.address();
        assert!(addr.starts_with("0x"), "EVM address must start with 0x: {addr}");
        assert_eq!(addr.len(), 42, "EVM address must be 42 chars: {addr}");
    }

    #[test]
    fn evm_signer_hex_roundtrip() {
        let signer1 = EvmChainSigner::random(1);
        let hex = signer1.to_hex_key();
        let signer2 = EvmChainSigner::from_hex(&hex, 1).unwrap();
        assert_eq!(signer1.address(), signer2.address());
    }

    #[test]
    fn chain_family_is_evm_and_is_solana() {
        assert!(ChainFamily::Solana.is_solana());
        assert!(!ChainFamily::Solana.is_evm());
        assert!(ChainFamily::Evm { chain_id: 1 }.is_evm());
        assert!(!ChainFamily::Evm { chain_id: 1 }.is_solana());
    }
}
```

---

## `rust/crates/core/src/lib.rs` 수정

```rust
// 기존 모듈들 유지 ...
pub mod accounts;
pub mod config;
pub mod error;
pub mod instructions;
pub mod keystore;
pub mod signer;
pub mod skills;

// [신규] — `evm` feature가 켜졌을 때만 컴파일됨
#[cfg(feature = "evm")]
pub mod chain;

pub mod client;
pub mod server;
// ...
```

이렇게 모듈 선언 라인에만 `#[cfg(feature = "evm")]`를 붙이면 `chain.rs` 본문에는 내부 `cfg` 표기가 전혀 필요 없다(파일 자체가 통째로 gated됨).

---

## 검증

```bash
# 컴파일 확인
cargo build -p pay-core

# 단위 테스트
cargo test -p pay-core chain

# 예상 통과 테스트
# chain::tests::chain_family_from_slug
# chain::tests::chain_family_from_caip2
# chain::tests::evm_signer_random_has_valid_address
# chain::tests::evm_signer_hex_roundtrip
```

---

## 다음 단계

Phase 2: [EVM 계정 레지스트리 확장](./02-phase2-account-registry.md)
