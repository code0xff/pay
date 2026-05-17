# Phase 15: EVM x402 Skills Index 노출

## 배경

Phase 3–13 으로 EVM x402 **결제 경로**(클라이언트 서명·서버 facilitator)는
완성됐다. 그러나 `pay-skills` 인덱스를 빌드하는 `skills::probe` 와
`skills::build` 는 여전히 **Solana 전용**으로 동작한다.

- `probe::extract_paid_endpoint` (probe.rs:215–244) 는 `accepts[]` 에서
  `is_solana_network(network)` 인 항목만 walk 한다. EVM accepts 는
  `paid.protocols`/`supported_usd`/`recipients` 어디에도 반영되지 않는다.
- `probe::classify_outcome` 의 X402Challenge 분기는 (Phase 14.x 회귀
  수정 직후 기준) Solana entry 가 없으면 `WrongChain` 을 반환해 인덱스에서
  탈락시킨다.
- `PaidEndpoint` (probe.rs:108–127) 자체에 **체인/네트워크 정보 슬롯**이
  없다. `recipients`, `price_usd`, `supported_usd` 가 모두 flat 이라 멀티체인
  endpoint 의 어느 entry 가 어느 네트워크인지 식별할 수 없다.

결과: Base 메인넷 USDC 만 받는 x402 서버, Sepolia 테스트 서버,
ethereum/base/optimism 멀티체인 advertising 서버 모두 인덱스에 등재되지
않고 따라서 다운스트림(MCP 카탈로그, pdb 디버거, 스킬 UI)에서 발견조차
되지 않는다.

별도 Phase 로 분리한 이유: **인덱스 스키마 변경**이 발생하므로
`pay-skills` 빌더, 인덱스 소비자(MCP·pdb·웹 UI) 모두 함께 갱신해야 하기
때문이다. EVM 결제 경로 변경 없이 인덱싱만 닫는다.

근거 파일:
- `rust/crates/core/src/skills/probe.rs:108-127, 215-244, 520-580`
- `rust/crates/core/src/skills/build.rs:849-913`
- `rust/crates/core/src/client/balance.rs:455-490` (stablecoin lookup)

---

## 목표

EVM-only / 멀티체인 x402 endpoint 가 인덱스에 **정상 등재**되고, 인덱스
소비자가 **올바른 체인을 선택**할 수 있는 메타데이터를 함께 노출한다.

비목표:
- 새 결제 흐름 추가 없음 (Phase 3 의 dispatcher 그대로 사용)
- v1 EVM 지원 없음 (Phase 14 분리)

---

## 원칙

1. **하위 호환** — 기존 `paid.recipients` / `price_usd` / `supported_usd` 는
   유지하고 모든 chain offer 의 union 으로 채운다. 새 정보는
   `paid.chain_offers` 로 추가한다.
2. **단일 walker** — `extract_paid_endpoint` 가 Solana/EVM 모두 처리한다.
   "EVM 전용 분기" 를 별도 함수로 두면 메타데이터 종류가 늘 때 한쪽이 누락된다.
3. **CAIP-2 정규화** — 인덱스에 저장되는 network 는 항상 CAIP-2
   (`solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`, `eip155:8453`). pay 슬러그
   (`mainnet`, `base`) 는 표시용으로만 쓰고 직렬화에 섞지 않는다.
4. **Symbol 역조회는 정적 1회 빌드** — `evm_stablecoin_address(symbol,
   chain_id)` 테이블을 `OnceLock<HashMap<(chain_id, Address), &'static str>>`
   로 한 번만 invert.

---

## 15-1. `PaidEndpoint` 스키마 확장

`crates/core/src/skills/probe.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize)]
pub struct PaidEndpoint {
    // ── 기존 필드 (호환 유지) ──────────────────────────────────────────
    pub protocols: Vec<String>,         // 모든 chain_offers union
    pub supported_usd: Vec<String>,     // 모든 chain_offers union
    pub price_usd: Option<f64>,         // 가장 싼 chain_offer
    pub recipients: Vec<String>,        // 모든 chain_offers union
    pub description: Option<String>,
    pub siwx_required: bool,

    // ── 신규 ─────────────────────────────────────────────────────────
    /// per-(network, asset) advertised offer. 멀티체인/멀티 토큰 서버는
    /// 여러 entry를 갖는다. 빈 벡터면 직렬화에서 생략 — 옛 인덱스 소비자가
    /// 새 필드를 무시할 수 있도록.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain_offers: Vec<ChainOffer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainOffer {
    /// CAIP-2 — `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`,
    /// `eip155:8453`, etc. 항상 표준 형식; 빈 문자열 금지.
    pub network: String,
    /// 정규화된 토큰 심볼 (USDC, USDT, …).
    pub currency: String,
    /// 토큰 컨트랙트 주소 (Solana: SPL mint, EVM: ERC-20 address).
    pub asset: String,
    /// 결제 받는 주소.
    pub recipient: String,
    /// 결제 base unit 정수 문자열 (no decimals).
    pub amount_raw: String,
    /// `amount_raw` 를 token decimals 로 나눈 USD 환산. None 이면
    /// decimals 매핑이 없어 변환 불가능했음.
    pub price_usd: Option<f64>,
}
```

기존 flat 필드는 derive: `chain_offers.iter()` 순회로 채운다. 옛 인덱스
소비자는 flat 필드만 보면 되고, 새 소비자는 `chain_offers` 를 읽어
체인을 선택한다.

---

## 15-2. `extract_paid_endpoint` 멀티체인 walker

현재 (probe.rs:215–244) Solana 만 walk. 변경 후:

```rust
for accept in accepts {
    let network = accept.get("network").and_then(|v| v.as_str()).unwrap_or("");
    let asset = accept.get("asset").and_then(|v| v.as_str()).unwrap_or("");
    let amount_str = accept.get("amount").and_then(|v| v.as_str()).unwrap_or("0");
    let recipient = accept.get("payTo").and_then(|v| v.as_str()).unwrap_or("");

    let (caip2, symbol, decimals) = match resolve_offer(network, asset) {
        Some(t) => t,
        None => continue,  // 모르는 체인/토큰 — skip
    };
    if !is_usd_stable(&symbol) { continue; }

    let price_usd = amount_to_usd(amount_str, decimals);
    paid.chain_offers.push(ChainOffer {
        network: caip2,
        currency: symbol.clone(),
        asset: asset.to_string(),
        recipient: recipient.to_string(),
        amount_raw: amount_str.to_string(),
        price_usd,
    });
    push_unique(&mut paid.supported_usd, &symbol);
    push_unique(&mut paid.recipients, recipient);
    if let Some(p) = price_usd {
        update_canonical_price(&mut paid.price_usd, p, &symbol);
    }
}
if !paid.chain_offers.is_empty() {
    push_unique(&mut paid.protocols, "x402");
}
```

`resolve_offer(network, asset)` 헬퍼:
- `is_solana_network(network)` → `("solana:…", normalize_currency(asset), decimals_for(symbol))`
- `network.starts_with("eip155:")` → `(network.to_string(), evm_symbol_for(chain_id, asset)?, evm_stablecoin_decimals(symbol)?)`
- 그 외 → `None`

---

## 15-3. `classify_outcome` X402 분기 완화

Phase 14.x 회귀 수정에서 Solana entry 없으면 `WrongChain` 반환. 본 Phase 에서
풀어준다:

```rust
RunOutcome::X402Challenge { challenge, .. } => {
    // chain_offers 가 있으면 그중 선호하는 entry 선택, 없으면 WrongChain
    let candidates: &[_] = if challenge.all_accepts.is_empty() {
        std::slice::from_ref(&challenge.requirements)
    } else { &challenge.all_accepts };

    // 우선순위: Solana → EVM (any known stable). 둘 다 없으면 WrongChain.
    let chosen = pick_indexable_x402(candidates);
    let Some(chosen) = chosen else {
        return ProbeStatus::WrongChain {
            details: format!("no indexable accepts: {}", caip2_list(candidates)),
        };
    };

    let currency = normalize_currency(&chosen.currency);
    let network = caip2_of(chosen);
    if !accepted.iter().any(|a| a.eq_ignore_ascii_case(&currency)) {
        return ProbeStatus::WrongCurrency { got: currency, accepted: accepted.to_vec() };
    }
    ProbeStatus::Ok { protocol: "x402".into(), currency, network, recipient: chosen.recipient.clone() }
}
```

`pick_indexable_x402` — **EVM-first when `evm` feature is on**:
1. `cfg(feature = "evm")` 빌드에서 `network.starts_with("eip155:")` && asset →
   stable symbol 매핑이 OK 인 첫 entry. EVM 빌드는 명시적 opt-in 이므로
   EVM 결제 경로가 곧바로 동작 가능한 후보를 인덱스 primary 로 노출한다.
2. `is_solana_network(network)` 인 첫 entry (Solana-only 빌드 또는 EVM
   entry 가 없을 때).
3. None.

`paid.chain_offers` 자체는 모든 known stable accepts 를 다 담으므로,
`ProbeStatus::Ok::network` 가 어느 쪽을 가리키든 다운스트림은 전체 옵션을
볼 수 있다. 이 우선순위는 **legacy flat 필드 소비자에 대한 표시 기본값**일
뿐이다.

---

## 15-4. EVM Symbol Reverse Lookup

`crates/core/src/client/balance.rs` 의 `evm_stablecoin_address(network, symbol)`
은 forward 매핑. 본 Phase 는 역방향 lookup 필요.

신규 함수:
```rust
pub fn evm_symbol_for(chain_id: u64, address: &str) -> Option<&'static str>;
```

구현:
```rust
static EVM_ADDR_LOOKUP: OnceLock<HashMap<(u64, alloy::primitives::Address), &'static str>> = OnceLock::new();

fn lookup_table() -> &'static HashMap<(u64, Address), &'static str> {
    EVM_ADDR_LOOKUP.get_or_init(|| {
        let mut m = HashMap::new();
        // forward table 을 invert
        for (slug, chain_id) in EVM_NETWORK_TABLE.iter() {
            for sym in EVM_STABLES {  // ["USDC", "USDT", "DAI", ...]
                if let Some(addr) = evm_stablecoin_address(slug, sym) {
                    if let Ok(a) = addr.parse::<Address>() {
                        m.insert((*chain_id, a), *sym);
                    }
                }
            }
        }
        m
    })
}
```

대소문자/0x prefix 정규화는 alloy `Address::from_str` 가 처리.
`extract_paid_endpoint` 에서 `network` 가 `eip155:<id>` 형태이므로
chain_id 파싱은 `split_once(':')` 한 줄.

---

## 15-5. 인덱스 소비자 (downstream) 영향

| 컴포넌트 | 현재 가정 | Phase 15 이후 처리 |
|---------|----------|------------------|
| `pay` CLI (`pay curl`) | 인덱스 미사용 | 영향 없음 |
| `pay skills build` | Solana entry 만 publish | 멀티체인 모두 publish, `chain_offers` 포함 |
| MCP 카탈로그 노출 | flat `recipients[0]` 가 Solana 가정 | `chain_offers` 우선, fallback flat |
| `pdb` 디버거 | 인덱스 recipient 로 결제 시도 | `chain_offers` 노출 → 사용자가 chain 선택 후 결제 |
| 웹 UI / 스킬 카탈로그 | "USDC on Solana" 칩 | 칩에 chain 표시 (`USDC on Base`, `USDC on Solana`) |

`paid.chain_offers` 가 빈 벡터면 직렬화 생략하므로 옛 인덱스 파일과의
호환은 유지된다 (역방향 호환은 옛 소비자가 신규 필드를 무시하는 형태).

---

## 15-6. 테스트

`crates/core/src/skills/probe.rs` 의 기존 테스트 옆에:

- `extract_paid_endpoint_walks_evm_accepts` — eip155:8453 USDC 응답이
  `chain_offers` 1건 + `protocols=["x402"]` 로 매핑
- `extract_paid_endpoint_skips_unknown_evm_token` — 모르는 ERC-20 주소는
  skip (chain_offers 비어 있음, protocols 비어 있음)
- `extract_paid_endpoint_merges_solana_and_evm` — 같은 envelope 가
  `solana:…` + `eip155:8453` 동시 advertise → chain_offers 2건, flat
  `supported_usd=["USDC"]`
- `classify_outcome_accepts_evm_only_x402_when_stable_symbol_known` —
  Phase 14.x 회귀 수정의 반대 시나리오
- `paid_endpoint_serde_omits_empty_chain_offers` — 옛 인덱스 소비자
  호환 검증

`crates/core/src/skills/build.rs` 의 probe server 픽스처 (build.rs:1064–
1083) 에 `/evm-only` (`eip155:8453` USDC) 와 `/multichain` 경로 추가.
`build_keeps_solana_paid_and_free_200_endpoints` 의 EVM-only 케이스는 별도
테스트로 분리하고 (현재 expect=[/paid,/free]) `/evm-only` 도 publish 되는
것을 확인하는 새 테스트 추가.

---

## 변경 파일

| 파일 | 변경 |
|------|------|
| `crates/core/src/skills/probe.rs` | `PaidEndpoint::chain_offers`, `ChainOffer`, walker 확장, classify_outcome 완화 |
| `crates/core/src/client/balance.rs` | `evm_symbol_for(chain_id, address)` + reverse lookup table |
| `crates/core/src/skills/build.rs` | probe server 픽스처 (`/evm-only`, `/multichain`), 신규 테스트 |
| (downstream 별도 PR) | MCP/pdb/UI 가 `chain_offers` 소비 |

`pay-types` 는 건드리지 않는다 — `PaidEndpoint` 는 `pay-core::skills::probe` 에
정의돼 있고 `pay-types` 로 이전된 적이 없다. 향후 `pay-types` 로 이동할
계획이라면 그 때 함께 처리.

---

## 우선순위

**P1** — EVM x402 결제 자체는 동작하므로 차단 상황은 아니지만, 인덱스에
없으면 사용자가 EVM 서버를 발견할 경로가 사라진다. 운영 베타 출시 전
닫는 게 좋다.

---

## Phase 종료 조건

- [ ] `PaidEndpoint::chain_offers` + `ChainOffer` 직렬화 round-trip 테스트
- [ ] `extract_paid_endpoint` EVM walker 단위 테스트 (USDC/USDT, 알 수
      없는 토큰 skip)
- [ ] `classify_outcome` EVM-only x402 → `Ok` (USDC accepted)
- [ ] `build_detail_endpoints` 가 `/evm-only` 픽스처를 `detail` 에 포함
- [ ] Solana-only 인덱스 빌드 회귀 없음 (`build_keeps_solana_paid_and_free_200_endpoints`)
- [ ] downstream consumer 마이그레이션 PR 트래킹 (MCP/pdb/UI)
- [ ] `CLAUDE.md` 강화 트랙 표에 Phase 15 추가
