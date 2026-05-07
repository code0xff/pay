# Phase 3: x402 멀티체인 파싱·서명

## 목표

`x402.rs`의 Solana 하드코딩을 제거하고, `accepts` 배열 전체를 파싱한 뒤  
지갑 설정에 맞는 체인을 선택해 Solana 또는 EVM 서명을 분기한다.  
신규 `evm.rs`에 EVM x402 결제 헤더 빌더를 구현한다.

**사용 라이브러리:**
- `x402-chain-eip155 = "1.4.4"` — Coinbase 공식 EVM x402 타입/서명 크레이트
- `alloy = "1.7.3"` (features: `signer-local`, `eip712`) — EIP-712 서명

---

## 3-1. `x402.rs` — `Challenge` 구조체 확장

```rust
// Before
#[derive(Debug, Clone)]
pub struct Challenge {
    pub x402_version: u64,
    pub requirements: PaymentRequirements,
    pub siwx: Option<SiwxExtension>,
}

// After: all_accepts 필드 추가
#[derive(Debug, Clone)]
pub struct Challenge {
    pub x402_version: u64,
    pub requirements: PaymentRequirements,
    /// Full list from the server's `accepts` array; used for multi-chain selection.
    pub all_accepts: Vec<PaymentRequirements>,
    pub siwx: Option<SiwxExtension>,
}
```

`parse()` 함수 수정 — `all_accepts` 보존:

```rust
pub fn parse(headers: &[(String, String)], body: Option<&str>) -> Option<Challenge> {
    // None → 모든 accepts 항목 파싱 (기존: Some(SOLANA_MAINNET) 하드 선택 제거)
    let envelope = parse_payment_required_envelope(headers, body)?;
    let all_accepts = envelope.accepts.clone();

    // 기본값: Solana 항목 우선 선택 (하위 호환)
    let requirements =
        parse_x402_challenge_for_network(headers, body, Some(SOLANA_MAINNET))
        .or_else(|| envelope.accepts.into_iter().next())?;

    let siwx = parse_siwx_extension(headers, body).ok().flatten();
    Some(Challenge {
        x402_version: detect_x402_version(headers, body),
        requirements,
        all_accepts,
        siwx,
    })
}
```

---

## 3-2. `normalize_network()` — EVM CAIP-2 매핑 추가

```rust
fn normalize_network(raw: &str) -> String {
    match raw {
        // Solana CAIP-2 genesis hashes (기존 유지)
        SOLANA_MAINNET | "solana" | "mainnet-beta" => "mainnet".to_string(),
        SOLANA_DEVNET | "solana-devnet" => "devnet".to_string(),
        SOLANA_TESTNET | "solana-testnet" => "testnet".to_string(),

        // [신규] EVM CAIP-2
        "eip155:1"        => "ethereum".to_string(),
        "eip155:8453"     => "base".to_string(),
        "eip155:10"       => "optimism".to_string(),
        "eip155:42161"    => "arbitrum".to_string(),
        "eip155:11155111" => "sepolia".to_string(),
        "eip155:17000"    => "holesky".to_string(),
        "eip155:84532"    => "base-sepolia".to_string(),

        // Already a slug or unknown — pass through
        other => other.to_string(),
    }
}
```

---

## 3-3. `select_best_chain()` 신규 함수

지갑 설정(`AccountsStore`)을 기반으로 `accepts` 배열에서 최적 체인을 선택한다.

```rust
/// Select the best `PaymentRequirements` entry from the server's `accepts` list.
///
/// Priority:
///   1. `network_override` CLI flag (--network) — picks the first accepts entry
///      whose normalized network slug matches.
///   2. First accepts entry whose network the local wallet has an account for.
///   3. First Solana entry (backward-compatible default).
///   4. First entry overall (last resort).
pub fn select_best_chain<'a>(
    accepts: &'a [PaymentRequirements],
    store: &dyn AccountsStore,
    network_override: Option<&str>,
) -> Option<&'a PaymentRequirements> {
    use crate::accounts::ChainFamily;

    if accepts.is_empty() {
        return None;
    }

    // 1. CLI --network override
    if let Some(override_slug) = network_override {
        if let Some(r) = accepts
            .iter()
            .find(|r| normalize_network(&r.network) == override_slug
                   || normalize_network(r.cluster.as_deref().unwrap_or("")) == override_slug)
        {
            return Some(r);
        }
    }

    // 2. First entry whose network has a configured wallet
    let file = store.load().ok()?;
    if let Some(r) = accepts.iter().find(|r| {
        let slug = normalize_network(&r.network);
        file.named_account_for_network(&slug, "default").is_some()
    }) {
        return Some(r);
    }

    // 3. Prefer Solana (backward compat)
    if let Some(r) = accepts.iter().find(|r| {
        let slug = normalize_network(&r.network);
        !crate::accounts::is_evm_network_family(&slug)
    }) {
        return Some(r);
    }

    // 4. Fallback: first entry
    accepts.first()
}
```

---

## 3-4. `build_payment()` — 체인별 분기

```rust
pub fn build_payment(
    challenge: &Challenge,
    store: &dyn AccountsStore,
    network_override: Option<&str>,
    account_override: Option<&str>,
    resource_url: Option<&str>,
) -> Result<BuiltPayment> {
    // [신규] all_accepts가 있으면 최적 체인 재선택
    let requirements = if !challenge.all_accepts.is_empty() {
        select_best_chain(&challenge.all_accepts, store, network_override)
            .unwrap_or(&challenge.requirements)
    } else {
        &challenge.requirements
    };

    let cluster = normalize_network(
        requirements.cluster.as_deref().unwrap_or(requirements.network.as_str()),
    );
    let network = network_override.map(str::to_string).unwrap_or(cluster.clone());

    // [신규] EVM 체인 분기
    if crate::accounts::is_evm_network_family(&network) {
        return crate::client::evm::build_evm_payment(
            challenge,
            requirements,
            &network,
            store,
            account_override,
        );
    }

    // 기존 Solana 경로 유지 (아래 코드 변경 없음)
    let amount = format_amount(&requirements.amount, &requirements.currency);
    // ... 기존 Solana 로직 ...
}
```

---

## 3-5. 신규 파일: `rust/crates/core/src/client/evm.rs`

`x402-chain-eip155` 크레이트를 사용해 EVM x402 결제 헤더를 빌드한다.

### 의존성 확인

```toml
# Cargo.toml (이미 Phase 1에서 추가됨)
x402-chain-eip155 = { workspace = true }
alloy = { workspace = true }
```

### 전체 구현

```rust
//! EVM x402 payment builder.
//!
//! Uses `x402-chain-eip155` (Coinbase official) for EIP-712 typed-data
//! signing and payload construction. `alloy` provides the low-level
//! PrivateKeySigner.

use crate::accounts::AccountsStore;
use crate::client::x402::Challenge;
use crate::client::x402::BuiltPayment;
use crate::{Error, Result};
use solana_x402::exact::PaymentRequirements;

/// Build EVM x402 payment headers using `x402-chain-eip155`.
///
/// Produces a `PAYMENT-SIGNATURE` header signed with EIP-712 / ERC-3009
/// `transferWithAuthorization`.
pub fn build_evm_payment(
    challenge: &Challenge,
    requirements: &PaymentRequirements,
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
) -> Result<BuiltPayment> {
    use crate::chain::{ChainFamily, EvmChainSigner};
    use crate::signer::load_evm_signer_for_network;

    let chain_id = match ChainFamily::from_network_slug(network) {
        ChainFamily::Evm { chain_id } => chain_id,
        _ => return Err(Error::Config(format!("`{network}` is not an EVM network"))),
    };

    let (evm_signer, ephemeral_notice) =
        load_evm_signer_for_network(network, store, account_override)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create runtime: {e}")))?;

    let header_value = rt
        .block_on(build_evm_payment_header(&evm_signer, requirements, chain_id))
        .map_err(|e| Error::Mpp(format!("Failed to build EVM x402 payment: {e}")))?;

    tracing::info!(
        network = %network,
        chain_id = %chain_id,
        address = %evm_signer.address(),
        "Built EVM x402 payment"
    );

    Ok(BuiltPayment {
        headers: vec![(crate::client::x402::X402_V2_PAYMENT_HEADER, header_value)],
        ephemeral_notice,
    })
}

/// Low-level: sign a `PaymentRequirements` with EIP-712 via `x402-chain-eip155`.
///
/// `x402-chain-eip155` exposes `build_payment_header` which handles:
///   - ERC-3009 `transferWithAuthorization` typed data construction
///   - EIP-712 domain separator (verifyingContract = token address, chainId)
///   - secp256k1 signing via the provided alloy `PrivateKeySigner`
///   - Base64url-encoded JSON payload → `PAYMENT-SIGNATURE` header value
async fn build_evm_payment_header(
    signer: &crate::chain::EvmChainSigner,
    requirements: &PaymentRequirements,
    chain_id: u64,
) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use x402_chain_eip155::client::build_payment_header;

    // x402-chain-eip155 expects alloy PrivateKeySigner directly.
    // EvmChainSigner wraps it as `pub signer: alloy::signers::local::PrivateKeySigner`.
    build_payment_header(&signer.signer, requirements).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, AccountsFile, Keystore, MemoryAccountsStore};
    use crate::chain::EvmChainSigner;

    fn sepolia_account(signer: &EvmChainSigner) -> Account {
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
            created_at: None,
        }
    }

    #[test]
    fn build_evm_payment_returns_payment_signature_header() {
        use solana_x402::exact::PaymentRequirements;

        let signer = EvmChainSigner::random(11155111);
        let mut file = AccountsFile::default();
        file.upsert("sepolia", "default", sepolia_account(&signer));
        let store = MemoryAccountsStore::with_file(file);

        let requirements = PaymentRequirements {
            network: "eip155:11155111".to_string(),
            recipient: "0xrecipient000000000000000000000000000000".to_string(),
            asset: "0xusdc00000000000000000000000000000000000".to_string(),
            amount: "1000000".to_string(),
            currency: "USDC".to_string(),
            description: None,
            resource: "https://example.com/resource".to_string(),
            cluster: None,
            max_timeout_seconds: None,
            mime_type: None,
            output_schema: None,
            extra: Default::default(),
        };

        let challenge = crate::client::x402::Challenge {
            x402_version: solana_x402::X402_VERSION_V2,
            requirements: requirements.clone(),
            all_accepts: vec![requirements.clone()],
            siwx: None,
        };

        let result = build_evm_payment(&challenge, &requirements, "sepolia", &store, None);
        assert!(result.is_ok(), "EVM payment build failed: {:?}", result.err());
        let built = result.unwrap();
        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].0, "PAYMENT-SIGNATURE");
        assert!(!built.headers[0].1.is_empty());
    }
}
```

---

## 3-6. `lib.rs` — `client::evm` 모듈 등록

```rust
// rust/crates/core/src/client/mod.rs (또는 lib.rs)
pub mod evm;  // ← 추가
```

---

## 3-7. `accounts.rs` — `is_evm_network_family` 공개 함수 확인

Phase 2에서 추가한 함수가 `pub`인지 확인:

```rust
// accounts.rs
pub fn is_evm_network_family(network: &str) -> bool {
    matches!(network,
        "ethereum" | "base" | "optimism" | "arbitrum"
        | "sepolia" | "holesky" | "base-sepolia"
    )
}
```

---

## 검증

```bash
# 빌드 확인
cargo build -p pay-core

# 단위 테스트
cargo test -p pay-core evm
cargo test -p pay-core x402

# 예상 통과 테스트
# client::evm::tests::build_evm_payment_returns_payment_signature_header
# client::x402::tests::normalize_network_maps_eip155_to_slugs
# client::x402::tests::select_best_chain_prefers_configured_wallet
```

---

## 다음 단계

Phase 4: [Runner EVM 거부 코드 제거](./04-phase4-runner-cleanup.md)
