# Phase 19: 체인/토큰 확장

## 배경

EVM 멀티체인 인프라 (Phase 1–17) 가 완성된 만큼 실제 운영 환경에서 자주
요청되는 체인/토큰을 채워 넣는다. 현재 지원:

- **체인 7개**: ethereum, base, optimism, arbitrum, sepolia, holesky, base-sepolia
- **토큰 2개**: USDC (모든 체인), USDT (ethereum 만)
- **Decimals 매핑**: USDC=6, USDT=6, DAI=18 (DAI 는 매핑만 있고 주소 없음)

지원되지 않는 자주 마주치는 케이스:
- **Polygon PoS** — Coinbase x402 facilitator 공식 지원 체인
- **Avalanche C-Chain** — USDC native 거래량 큼
- **Linea** — Consensys L2, x402 생태계 확장 타겟
- **PYUSD on Ethereum** — PayPal 발행, 점차 채택
- **DAI on Ethereum** — 주소 매핑이 없어 실제로는 사용 불가
- **Polygon Amoy** — Polygon 의 새 테스트넷 (mumbai 종료 이후)

## 목표

체인 4개, 토큰 2개 (실제 사용 가능 매핑) 를 추가하고 reverse lookup 테이블에
반영한다. 모든 추가는 단일 enumeration 사이트에서만 일어나도록
single-source-of-truth 패턴을 유지한다.

비목표:
- Solana 토큰 확장 — 별도 트랙.
- L2 별 ETH gas estimation — 결제 경로에 영향 없음.
- 자체 RPC 운영 — 본 phase 는 publicnode.com 공개 endpoint 사용.

## 원칙

1. **순수 add-only** — 기존 체인/토큰의 chain_id 나 주소를 바꾸지
   않는다 (운영 호환).
2. **단일 enumeration 사이트** — 각 정보 (slug↔chain_id, 토큰 주소,
   decimals, RPC default) 마다 하나의 함수/테이블에서만 분기.
   reverse lookup 은 자동 invert.
3. **테스트넷도 lazy** — 신규 추가 테스트넷 (`amoy`) 도
   `is_evm_lazy_network` 에 포함 → ephemeral wallet 자동 생성.
4. **Decimals 정확성 > 커버리지** — 잘못된 decimals 는 underpaid 발생.
   확인된 토큰만 추가, 의심스러우면 미포함.

---

## 19-1. 신규 체인

| slug | chain_id | family | RPC default | 비고 |
|------|----------|--------|------------|------|
| polygon | 137 | mainnet | polygon-bor-rpc.publicnode.com | PoS |
| avalanche | 43114 | mainnet | avalanche-c-chain-rpc.publicnode.com | C-Chain |
| linea | 59144 | mainnet | linea-rpc.publicnode.com | L2 |
| amoy | 80002 | testnet (lazy) | polygon-amoy-bor-rpc.publicnode.com | Polygon testnet |

### 수정 지점

- `accounts.rs::is_evm_network_family` — slug 추가
- `accounts.rs::is_evm_lazy_network` — amoy 추가
- `chain.rs::ChainFamily::from_network_slug` / `to_network_slug` — 양방향
- `balance.rs::evm_default_rpc_url` — 신규 endpoint
- `balance.rs::EVM_NETWORKS` (Phase 15) — chain_id 추가
- CLAUDE.md 네트워크 슬러그 표 갱신

---

## 19-2. 신규 토큰

| symbol | network | address | decimals | 비고 |
|--------|---------|---------|----------|------|
| USDC | polygon | 0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359 | 6 | native PoS USDC |
| USDC | avalanche | 0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E | 6 | native C-Chain USDC |
| USDC | linea | 0x176211869cA2b568f2A7D4EE941E073a821EE1ff | 6 | bridged |
| USDC | amoy | 0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582 | 6 | testnet |
| DAI | ethereum | 0x6B175474E89094C44Da98b954EedeAC495271d0F | 18 | MakerDAO |
| PYUSD | ethereum | 0x6c3ea9036406852006290770BEdFcAbA0e23A0e8 | 6 | PayPal USD |

### 수정 지점

- `balance.rs::evm_stablecoin_address` — (chain, sym) → address
- `balance.rs::evm_stablecoin_decimals` — PYUSD 추가 (DAI/USDC/USDT 기존)
- `balance.rs::EVM_STABLE_SYMBOLS` — `["USDC", "USDT", "DAI", "PYUSD"]`
- `client::x402::currency_preference` — DAI/PYUSD 도 known 으로 등록
  (others=99 → DAI=2, PYUSD=3 정도. USDC > USDT > DAI > PYUSD > rest)

---

## 19-3. Reverse lookup 자동 invert

`balance.rs::evm_symbol_for` 가 `EVM_NETWORKS × EVM_STABLE_SYMBOLS` 를
OnceLock 으로 invert. 본 phase 의 신규 항목들이 자동 포함된다 — 코드
변경 없음.

---

## 19-4. `currency_preference` 우선순위

```rust
fn currency_preference(symbol: &str) -> u8 {
    match symbol.to_ascii_uppercase().as_str() {
        "USDC" => 0,
        "USDT" => 1,
        "DAI" => 2,
        "PYUSD" => 3,
        _ => 99,
    }
}
```

이유: USDC 가 가장 광범위 채택, USDT 가 fallback, DAI 는 algorithmic,
PYUSD 는 발행자별 신뢰도 차이가 있어 default 에서는 마지막.

---

## 19-5. 테스트

- `evm_network_family_recognizes_phase19_chains` — polygon/avalanche/linea/amoy
- `evm_default_rpc_url_polygon_avalanche_linea_amoy`
- `evm_stablecoin_address_returns_polygon_usdc_etc` — 신규 6개 매핑
- `evm_stablecoin_decimals_recognizes_pyusd`
- `evm_symbol_for_reverses_polygon_usdc`
- `currency_preference_includes_dai_pyusd`
- `chain_family_from_network_slug_polygon_avalanche_linea_amoy`

---

## 변경 파일

| 파일 | 변경 |
|------|------|
| `crates/core/src/accounts.rs` | `is_evm_network_family`, `is_evm_lazy_network` |
| `crates/core/src/chain.rs` | `ChainFamily::from_network_slug`, `to_network_slug` |
| `crates/core/src/client/balance.rs` | `evm_default_rpc_url`, `evm_stablecoin_address`, `evm_stablecoin_decimals`, `EVM_NETWORKS`, `EVM_STABLE_SYMBOLS` |
| `crates/core/src/client/x402.rs` | `currency_preference` (DAI/PYUSD) |
| `CLAUDE.md` | 네트워크 슬러그 표 갱신 |

---

## 우선순위

P2 — 인프라는 이미 갖춰져 있어 코드 추가만 필요. 실 사용자 도달
범위에 따라 P1 으로 승격 가능.

## Phase 종료 조건

- [ ] 4 신규 체인 + 6 신규 (chain, token) 매핑 단위 테스트 통과
- [ ] reverse lookup 자동 포함 검증
- [ ] 회귀 없음
- [ ] CLAUDE.md 네트워크 슬러그 표 갱신
