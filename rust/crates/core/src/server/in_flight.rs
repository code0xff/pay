//! In-flight `(chain_id, from, nonce)` lock for the EVM x402 gateway.
//!
//! Why not an LRU?
//!
//! EIP-3009 records used nonces permanently in `_authorizationStates` on the
//! USDC contract. The contract is the authoritative source of truth, and the
//! gateway queries it (see `authorization_state` in `evm_x402_payment.rs`)
//! to reject *sequential* replays that already mined.
//!
//! There is, however, a gap: between the moment we kick off
//! `facilitator.settle` and the moment the resulting transaction is mined
//! (Ethereum ~12 s, Base ~2 s), `authorizationState(from, nonce)` still
//! returns `false`. A burst of identical envelopes in that window would each
//! pass the on-chain pre-check. A misbehaving facilitator that idempotently
//! returns the same tx hash for repeated envelopes would even let them all
//! satisfy Phase 11-1's receipt verification, because the tx really did
//! happen — just not for *this* request.
//!
//! `InFlight` closes that gap: while a `(chain_id, from, nonce)` is being
//! processed, no other request with the same key can acquire the slot. The
//! guard's `Drop` releases the slot once we know the on-chain outcome (the
//! authorization-state check or Phase 11-1 receipt verification has either
//! accepted or rejected the request).
//!
//! Capacity is unbounded but naturally tiny: the set only ever holds
//! currently-processing payments — at most `concurrent_requests`-many
//! entries. There is no LRU eviction, so a flood of fake envelopes cannot
//! evict a real entry to clear the way for a replay.

#![cfg(feature = "evm")]

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;

/// Composite key for an EIP-3009 authorization. `from` and `nonce` alone are
/// not unique across chains — a relayer running on multiple networks could
/// see the same `(from, nonce)` pair representing independent authorizations
/// — so we include `chain_id`.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct NonceKey {
    pub chain_id: u64,
    pub from: [u8; 20],
    pub nonce: [u8; 32],
}

impl NonceKey {
    /// Pull `chain_id` from `requirements.network` (`eip155:<id>`) and the
    /// `from`/`nonce` pair from the decoded x402 envelope. Returns a clear
    /// error string when any field is missing or malformed so the caller can
    /// surface a verification failure rather than a generic 500.
    pub fn from_envelope(
        payment_payload: &serde_json::Value,
        requirements: &serde_json::Value,
    ) -> Result<Self, String> {
        let network = requirements
            .get("network")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "requirements.network missing".to_string())?;
        let chain_id = network
            .strip_prefix("eip155:")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                format!("requirements.network `{network}` is not a CAIP-2 eip155 reference")
            })?;

        let auth = payment_payload
            .pointer("/payload/authorization")
            .ok_or_else(|| "payment payload missing payload.authorization".to_string())?;
        let from_hex = auth
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "authorization.from missing".to_string())?;
        let nonce_hex = auth
            .get("nonce")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "authorization.nonce missing".to_string())?;

        let from = parse_address(from_hex)
            .map_err(|e| format!("authorization.from `{from_hex}` is invalid: {e}"))?;
        let nonce = parse_b256(nonce_hex)
            .map_err(|e| format!("authorization.nonce `{nonce_hex}` is invalid: {e}"))?;

        Ok(Self {
            chain_id,
            from,
            nonce,
        })
    }
}

/// Per-(chain_id, from, nonce) processing lock backed by a `HashSet`.
#[derive(Debug, Default)]
pub struct InFlight {
    set: Arc<Mutex<HashSet<NonceKey>>>,
}

impl InFlight {
    pub fn new() -> Self {
        Self {
            set: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Try to take ownership of `key`. Returns `None` when another request is
    /// already processing the same `(chain_id, from, nonce)` triple. The
    /// returned guard releases the slot on `Drop`, including unwind paths.
    pub fn try_acquire(&self, key: NonceKey) -> Option<InFlightGuard> {
        let mut g = self.set.lock();
        if !g.insert(key) {
            return None;
        }
        Some(InFlightGuard {
            set: Arc::clone(&self.set),
            key,
        })
    }

    /// Current number of in-flight payments, exposed for telemetry / tests.
    pub fn len(&self) -> usize {
        self.set.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.lock().is_empty()
    }
}

/// RAII guard. Releasing it removes the key from the in-flight set so the
/// next request can take its turn.
pub struct InFlightGuard {
    set: Arc<Mutex<HashSet<NonceKey>>>,
    key: NonceKey,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set.lock().remove(&self.key);
    }
}

fn parse_address(s: &str) -> Result<[u8; 20], String> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    if stripped.len() != 40 {
        return Err(format!(
            "expected 20-byte hex (40 chars), got {}",
            stripped.len()
        ));
    }
    let bytes = hex::decode(stripped).map_err(|e| e.to_string())?;
    bytes
        .try_into()
        .map_err(|_| "hex decode produced wrong length".to_string())
}

fn parse_b256(s: &str) -> Result<[u8; 32], String> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    if stripped.len() != 64 {
        return Err(format!(
            "expected 32-byte hex (64 chars), got {}",
            stripped.len()
        ));
    }
    let bytes = hex::decode(stripped).map_err(|e| e.to_string())?;
    bytes
        .try_into()
        .map_err(|_| "hex decode produced wrong length".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_envelope() -> (serde_json::Value, serde_json::Value) {
        let payload = json!({
            "x402Version": 2,
            "scheme": "exact",
            "network": "eip155:11155111",
            "payload": {
                "signature": "0xdeadbeef",
                "authorization": {
                    "from": "0x1111111111111111111111111111111111111111",
                    "to":   "0x2222222222222222222222222222222222222222",
                    "value": "100000",
                    "validAfter": "0",
                    "validBefore": "9999999999",
                    "nonce": "0x0000000000000000000000000000000000000000000000000000000000000001"
                }
            }
        });
        let requirements = json!({
            "scheme": "exact",
            "network": "eip155:11155111",
            "asset": "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
            "payTo": "0x2222222222222222222222222222222222222222",
            "amount": "100000",
        });
        (payload, requirements)
    }

    #[test]
    fn from_envelope_extracts_chain_from_and_nonce() {
        let (payload, requirements) = sample_envelope();
        let key = NonceKey::from_envelope(&payload, &requirements).expect("parse");
        assert_eq!(key.chain_id, 11_155_111);
        assert_eq!(key.from[0], 0x11);
        assert_eq!(key.nonce[31], 0x01);
    }

    #[test]
    fn from_envelope_rejects_non_caip2_network() {
        let (payload, mut requirements) = sample_envelope();
        requirements["network"] = json!("solana:devnet");
        let err = NonceKey::from_envelope(&payload, &requirements).unwrap_err();
        assert!(err.contains("eip155"), "got: {err}");
    }

    #[test]
    fn from_envelope_rejects_missing_authorization() {
        let (mut payload, requirements) = sample_envelope();
        payload["payload"] = json!({});
        let err = NonceKey::from_envelope(&payload, &requirements).unwrap_err();
        assert!(err.contains("payload.authorization"), "got: {err}");
    }

    #[test]
    fn from_envelope_rejects_short_from() {
        let (mut payload, requirements) = sample_envelope();
        payload["payload"]["authorization"]["from"] = json!("0x1234");
        let err = NonceKey::from_envelope(&payload, &requirements).unwrap_err();
        assert!(err.contains("authorization.from"), "got: {err}");
    }

    #[test]
    fn from_envelope_rejects_short_nonce() {
        let (mut payload, requirements) = sample_envelope();
        payload["payload"]["authorization"]["nonce"] = json!("0xabcd");
        let err = NonceKey::from_envelope(&payload, &requirements).unwrap_err();
        assert!(err.contains("authorization.nonce"), "got: {err}");
    }

    fn key() -> NonceKey {
        NonceKey {
            chain_id: 8453,
            from: [0xab; 20],
            nonce: [0xcd; 32],
        }
    }

    #[test]
    fn try_acquire_blocks_parallel() {
        let in_flight = InFlight::new();
        let g1 = in_flight.try_acquire(key()).expect("first acquire");
        assert!(in_flight.try_acquire(key()).is_none());
        drop(g1);
    }

    #[test]
    fn guard_releases_on_drop() {
        let in_flight = InFlight::new();
        {
            let _g = in_flight.try_acquire(key()).expect("first acquire");
            assert_eq!(in_flight.len(), 1);
        }
        assert!(in_flight.is_empty());
        // After release the slot can be reacquired.
        let _g2 = in_flight.try_acquire(key()).expect("reacquire after drop");
    }

    #[test]
    fn different_chains_dont_collide() {
        let in_flight = InFlight::new();
        let k1 = NonceKey {
            chain_id: 1,
            ..key()
        };
        let k2 = NonceKey {
            chain_id: 8453,
            ..key()
        };
        let _g1 = in_flight.try_acquire(k1).expect("k1");
        let _g2 = in_flight.try_acquire(k2).expect("k2");
        assert_eq!(in_flight.len(), 2);
    }
}
