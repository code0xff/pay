# Phase 17: Server multi-accepts emission

## 배경

현재 `evm_x402_payment` 미들웨어는 **단일 (network, recipient)** 만 advertise
한다. 같은 운영자가 Base/Ethereum/Optimism 같이 여러 체인에서 결제를
받으려면 각 체인마다 별도 프로세스를 띄워야 한다.

`OperatorConfig` (types/metering.rs:561) 스키마:
- `network: Option<String>` — 단일 EVM 슬러그
- `recipient: Option<String>` — 단일 주소
- `rpc_url: Option<String>` — 단일 RPC
- `facilitator_url: Option<String>` — 단일 facilitator
- `currencies: Map<String, Vec<String>>` — 멀티 토큰만 지원 (Phase 13-4)

Phase 13-4 가 USDC+USDT 동시 advertising 까지는 처리하지만, 멀티 체인은
미지원. x402 스펙은 `accepts: [...]` 에 여러 체인을 동시에 넣는 걸 기본
가정한다 (예: Solana mainnet + Base + Ethereum 한 envelope).

## 목표

단일 서버 인스턴스가 **여러 EVM 체인 + 여러 토큰** 조합을 한 402 envelope
에 advertise 한다. 결제 수신 시 payment payload 의 chain_id 에 따라
해당 체인의 (recipient, rpc_url, facilitator_url) 로 검증한다.

비목표:
- Solana 와 EVM 혼합 advertising — 별도 phase (Solana SDK 가 단일 chain
  가정). Solana x402 + EVM x402 cross-mount 는 Phase 19+ 에서.
- 다중 candidate 클라이언트측 선택 — Phase 18 에서.

## 원칙

1. **하위 호환** — 기존 `operator.network` + `operator.recipient` 단일
   체인 운영자는 무변경. 신규 필드 `extra_evm_networks` 가 비어 있으면
   현 동작 그대로.
2. **단일 facilitator 우선** — 운영자가 모든 체인에 같은 facilitator 를
   쓰는 게 일반적. 체인별 override 는 옵션.
3. **체인 dispatch 는 payment payload 의 `network` 필드 기준** — payer 가
   서명한 envelope 의 `network: "eip155:<id>"` 가 진실. 매칭 안 되면
   verification_failed.
4. **운영자 입력 1회 검증** — boot 시 모든 (network, recipient, rpc_url,
   facilitator_url) 조합이 valid 한지 startup guard 가 확인. 런타임에
   silent skip 금지.

---

## 17-1. `OperatorConfig` 확장

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    // ... 기존 필드 ...

    /// Additional EVM chains advertised alongside the primary `network`.
    /// Each entry can override the operator-level recipient/rpc/facilitator
    /// for that chain. Empty by default — single-chain operators stay
    /// untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_evm_networks: Vec<EvmNetworkConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvmNetworkConfig {
    /// EVM network slug (`ethereum`, `base`, `optimism`, ...).
    pub network: String,
    /// Recipient address on this chain. If omitted, falls back to the
    /// operator-level `recipient` — useful when the same multisig is
    /// deployed at the same address on multiple chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Per-chain RPC URL. If omitted, falls back to the operator-level
    /// `rpc_url` (must be on the same chain — typically used for testnets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// Per-chain facilitator URL. If omitted, falls back to the
    /// operator-level `facilitator_url`. Most facilitators support all
    /// chains; per-chain override is for self-hosted setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facilitator_url: Option<String>,
}
```

---

## 17-2. Target table

서버 부팅 시 `(operator.network, extra_evm_networks)` 를 `Vec<EvmTarget>`
로 정규화:

```rust
struct EvmTarget {
    network: String,           // pay slug ("ethereum", "base", ...)
    chain_id: u64,             // resolved from slug
    recipient: String,         // resolved (per-chain or operator-level)
    rpc_url: String,
    facilitator_url: String,
}
```

Boot guard 거부 조건:
- `network` 가 `is_evm_network_family` 아님
- `recipient` 미설정 + operator-level 도 없음
- `rpc_url` 미설정 + operator-level 도 없음
- 동일 `chain_id` 중복
- Solana 슬러그 포함 시 (이 phase 의 범위 외)

---

## 17-3. Advertising loop

```rust
let targets = state.evm_targets();  // pre-validated at boot
let mut accepts = Vec::new();
for target in targets {
    for sym in &currency_symbols {
        match build_evm_requirements(
            &target.rpc_url, &target.network,
            &target.recipient, sym, amount_usd, &uri, description,
        ).await {
            Ok(req) => accepts.push(req),
            Err(e) => tracing::warn!(?target.network, %sym, %e, "skip"),
        }
    }
}
```

`accepts` 가 chain × currency 의 cartesian product. 운영자가 `[ethereum,
base, optimism]` × `[USDC, USDT]` 를 advertise 하면 6개 entry.

---

## 17-4. Receive dispatch

`handle_payment` 시그니처를 변경: `requirements` 단일이 아니라 `targets:
&[EvmTarget]` 를 받고 payment payload 의 `network` 로 lookup:

```rust
let payload_network = payment_payload
    .pointer("/payload/authorization/network")  // or top-level "network"
    .and_then(|v| v.as_str())
    .unwrap_or("");
let target = targets.iter().find(|t| {
    format!("eip155:{}", t.chain_id) == payload_network
});
let target = match target {
    Some(t) => t,
    None => return verification_failed_response(
        &format!("payment payload network `{payload_network}` not advertised"),
    ),
};
// rpc_url/recipient/facilitator 를 target 에서 가져와 verify + settle
```

`facilitator` 인스턴스도 chain 별로 분리되어야 함 (URL 다를 수 있음).
부팅 시 `BTreeMap<chain_id, FacilitatorClient>` 캐시.

---

## 17-5. 테스트

- `multi_chain_envelope_advertises_all_targets` — Base + Ethereum × USDC
  를 advertise → `accepts.len() == 2` (per chain) × currencies
- `handle_payment_dispatches_to_matching_chain` — Base 서명 payload 가
  들어오면 Base RPC/recipient 로 verify
- `handle_payment_rejects_payload_for_unadvertised_chain` — Polygon
  payload 가 들어오면 verification_failed
- `boot_rejects_duplicate_chain_id` — `extra_evm_networks` 에 같은 chain
  두 번 → startup error
- `boot_falls_back_to_operator_recipient_when_per_chain_omitted`
- `single_chain_legacy_config_still_works` — 회귀 가드

---

## 변경 파일

| 파일 | 변경 |
|------|------|
| `crates/types/src/metering.rs` | `EvmNetworkConfig`, `OperatorConfig::extra_evm_networks` |
| `crates/core/src/server/evm_x402_payment.rs` | target table 빌드, advertising loop, dispatch |
| `crates/cli/src/commands/server/evm_x402_start.rs` | boot guard 확장 |
| `crates/core/src/server/mod.rs` (혹은 `state.rs`) | per-chain facilitator + in_flight 캐시 |

---

## 우선순위

P1 — 운영자가 멀티체인 수익화를 원할 때 차단되는 단일 갭. EVM-first
라우팅(Phase 16) 다음의 자연스러운 확장.

## Phase 종료 조건

- [ ] `OperatorConfig::extra_evm_networks` 직렬화 round-trip
- [ ] Multi-chain advertising 단위 테스트
- [ ] Per-chain dispatch 단위 테스트
- [ ] Boot guard 거부 케이스 테스트
- [ ] 단일 체인 회귀 없음
- [ ] CLAUDE.md 강화 트랙 표에 Phase 17 추가
