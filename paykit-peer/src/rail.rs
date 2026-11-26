//! The peer's payment method: on-chain bitcoin on the local regtest node, through bitcoind's
//! wallet RPC. It follows the demo's payment method contract:
//!
//! | Item | Value |
//! |---|---|
//! | Receive identifier | `btc-regtest-p2wpkh` (`getnewaddress "" bech32`) |
//! | Identifiers it pays | `btc-regtest-p2wpkh`, `btc-regtest-p2tr` |
//! | Payload | `{"value":"<address>"}` |
//! | Amount | asset `btc`, decimal BTC, 8 fraction digits |
//! | Proof | `{"txid":"<64 lowercase hex>"}` |
//! | Settlement ID | the 32 bytes of the txid, in the order its hex is written |
//! | Verify | find the transaction, sum its outputs to the payload's address (or to any address of the payee's wallet when the payload isn't known), accept at or above the amount; unconfirmed is accepted |
//!
//! The payer sends at least the requested amount, with a fee on top at a fixed 2 sat/vB, so the
//! fee is bounded. Each payment carries the attempt ID in the wallet transaction's comment, so
//! the wallet's own history answers for an attempt after a restart, which makes paying
//! idempotent on the attempt ID.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_sdk::rail::ProofCheck;
use paykit_sdk::{
    EndpointSelection, PaymentEndpointCandidate, Rail, RailDeclaration, RailError, RailOutcome,
    RailPaid, RailPayment, Transfer,
};
use pubky_dispatch_client::transport::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::bitcoind::{btc_to_sats, sats_to_btc, Bitcoind, RpcError, INVALID_ADDRESS_OR_KEY};
use crate::state::unhex;

/// The identifier the peer receives on.
pub const P2WPKH: &str = "btc-regtest-p2wpkh";
/// A browser wallet's receive identifier (a Taproot address), which the peer can pay.
pub const P2TR: &str = "btc-regtest-p2tr";
/// The identifiers the peer pays.
const PAYS: [&str; 2] = [P2WPKH, P2TR];
/// The smallest unit: satoshis.
const FRACTION_DIGITS: u32 = 8;
/// Fee rate of every payment, sat/vB: a fixed bound on the fee.
const FEE_RATE: u64 = 2;
/// The prefix of a payment's wallet comment; the attempt ID follows.
const COMMENT_PREFIX: &str = "paykit-attempt:";
/// One handoff can take at most an hour to show an outcome.
const LONGEST_COMPLETION_MS: u64 = 3_600_000;
/// Clock tolerance between payer and payee.
const PAYMENT_TIME_TOLERANCE_MS: u64 = 300_000;
/// How far back the wallet history is searched for an attempt.
const HISTORY_SCAN: u64 = 1_000;

/// One handoff the rail made.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handoff {
    pub txid: String,
    pub paid_at_ms: u64,
    pub endpoint_id: String,
    pub address: String,
    pub sats: u64,
}

/// One proof the rail verified as a payee.
#[derive(Clone, Debug)]
pub struct Verified {
    pub txid: String,
    pub sats_received: u64,
}

pub struct RegtestRail {
    node: Bitcoind,
    wallet: String,
    /// Attempt ID (b64) -> handoff, kept in `history_path` when there is one.
    history: Mutex<BTreeMap<String, Handoff>>,
    history_path: Option<PathBuf>,
    /// Attempt ID (b64) -> the last proof verified for it.
    verified: Mutex<BTreeMap<String, Verified>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The address of a `{"value":"<address>"}` payload.
pub fn payload_address(payload: &PaymentEndpointPayload) -> Option<String> {
    let value: Value = serde_json::from_str(payload.as_str()).ok()?;
    let address = value.get("value")?.as_str()?;
    (!address.is_empty()).then(|| address.to_owned())
}

/// The payload of an address.
pub fn address_payload(address: &str) -> anyhow::Result<PaymentEndpointPayload> {
    Ok(PaymentEndpointPayload::new(
        &json!({ "value": address }).to_string(),
    )?)
}

/// Satoshis of a `btc` amount, rounded up to the next satoshi; `None` for another asset.
pub fn amount_sats(amount: &PaymentAmount) -> Option<u64> {
    if amount.asset.as_str() != "btc" {
        return None;
    }
    // Scaled by 10^18; a satoshi is 10^10.
    u64::try_from(amount.value.scaled().div_ceil(10_000_000_000)).ok()
}

/// The `btc` amount of `sats`, with 8 fraction digits.
pub fn btc_amount(sats: u64) -> anyhow::Result<PaymentAmount> {
    Ok(PaymentAmount {
        value: paykit_lib::Decimal::new(&sats_to_btc(sats))?,
        asset: paykit_lib::Asset::new("btc")?,
    })
}

/// The txid a proof names: 64 lowercase hex digits.
fn proof_txid(proof: &Map<String, Value>) -> Option<String> {
    let txid = proof.get("txid")?.as_str()?;
    let lower_hex = txid
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (txid.len() == 64 && lower_hex).then(|| txid.to_owned())
}

fn paid(handoff: &Handoff) -> RailOutcome {
    let mut proof = Map::new();
    proof.insert("txid".into(), Value::String(handoff.txid.clone()));
    RailOutcome::Paid(RailPaid::new(proof).with_paid_at_ms(handoff.paid_at_ms))
}

impl RegtestRail {
    pub fn new(
        node: Bitcoind,
        wallet: String,
        history_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let history = match &history_path {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => serde_json::from_slice(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(error) => return Err(error.into()),
            },
            None => BTreeMap::new(),
        };
        Ok(Self {
            node,
            wallet,
            history: Mutex::new(history),
            history_path,
            verified: Mutex::default(),
        })
    }

    pub fn node(&self) -> &Bitcoind {
        &self.node
    }

    pub fn wallet(&self) -> &str {
        &self.wallet
    }

    /// The handoff of `attempt`, if this rail made one.
    pub fn handoff(&self, attempt: &AttemptId) -> Option<Handoff> {
        lock(&self.history).get(&attempt.to_text()).cloned()
    }

    /// The last proof verified for `attempt`.
    pub fn verified(&self, attempt: &AttemptId) -> Option<Verified> {
        lock(&self.verified).get(&attempt.to_text()).cloned()
    }

    fn record(&self, key: String, handoff: Handoff) -> Result<(), RailError> {
        let mut history = lock(&self.history);
        history.insert(key, handoff);
        if let Some(path) = &self.history_path {
            let bytes = serde_json::to_vec_pretty(&*history)
                .map_err(|error| RailError::Unavailable(error.to_string()))?;
            crate::state::write_private(path, &bytes)
                .map_err(|error| RailError::Unavailable(format!("rail history: {error}")))?;
        }
        Ok(())
    }

    /// The wallet's own payment for attempt `key`, by its comment.
    async fn find_in_wallet(&self, key: &str) -> Result<Option<Handoff>, RailError> {
        let comment = format!("{COMMENT_PREFIX}{key}");
        let list = self
            .node
            .call(
                Some(&self.wallet),
                "listtransactions",
                json!(["*", HISTORY_SCAN, 0, true]),
            )
            .await
            .map_err(|error| RailError::Unavailable(error.to_string()))?;
        let found = list.as_array().and_then(|entries| {
            entries.iter().find(|entry| {
                entry.get("category").and_then(Value::as_str) == Some("send")
                    && entry.get("comment").and_then(Value::as_str) == Some(comment.as_str())
            })
        });
        Ok(found.and_then(|entry| {
            Some(Handoff {
                txid: entry.get("txid")?.as_str()?.to_owned(),
                paid_at_ms: entry.get("time")?.as_u64()?.saturating_mul(1000),
                endpoint_id: String::new(),
                address: entry
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                sats: entry
                    .get("amount")
                    .and_then(Value::as_f64)
                    .map(|btc| btc_to_sats(btc.abs()))
                    .unwrap_or(0),
            })
        }))
    }

    async fn pay_inner(&self, payment: RailPayment) -> Result<RailOutcome, RailError> {
        let key = payment.idempotency_key.to_text();
        let known = lock(&self.history).get(&key).cloned();
        if let Some(handoff) = known {
            return Ok(paid(&handoff));
        }
        if let Some(handoff) = self.find_in_wallet(&key).await? {
            self.record(key, handoff.clone())?;
            return Ok(paid(&handoff));
        }
        if !PAYS.contains(&payment.endpoint_id.as_str()) {
            return Err(RailError::Refused("unsupported endpoint".into()));
        }
        let address = payload_address(&payment.payload)
            .ok_or_else(|| RailError::Refused("the payload names no address".into()))?;
        let sats = amount_sats(&payment.amount)
            .ok_or_else(|| RailError::Refused("not a btc amount".into()))?;
        if sats == 0 {
            return Err(RailError::Refused("a zero amount".into()));
        }
        let sent = self
            .node
            .call(
                Some(&self.wallet),
                "sendtoaddress",
                json!({
                    "address": address,
                    "amount": sats_to_btc(sats),
                    "comment": format!("{COMMENT_PREFIX}{key}"),
                    "fee_rate": FEE_RATE,
                }),
            )
            .await;
        let txid = match sent {
            Ok(txid) => txid
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| RailError::Unavailable("sendtoaddress gave no txid".into()))?,
            // bitcoind refused before broadcasting: nothing was or will be submitted.
            Err(RpcError::Rpc { code, message }) => {
                return Err(RailError::Refused(format!(
                    "bitcoind error {code}: {message}"
                )))
            }
            // Not reached, or the answer was lost: the attempt stays open, and the wallet's
            // history answers for it later.
            Err(error) => return Err(RailError::Unavailable(error.to_string())),
        };
        let handoff = Handoff {
            txid,
            paid_at_ms: now_ms(),
            endpoint_id: payment.endpoint_id.as_str().to_owned(),
            address,
            sats,
        };
        self.record(key, handoff.clone())?;
        Ok(paid(&handoff))
    }

    async fn payment_inner(&self, attempt: AttemptId) -> Result<Option<RailOutcome>, RailError> {
        let key = attempt.to_text();
        let known = lock(&self.history).get(&key).cloned();
        if let Some(handoff) = known {
            return Ok(Some(paid(&handoff)));
        }
        match self.find_in_wallet(&key).await? {
            Some(handoff) => {
                self.record(key, handoff.clone())?;
                Ok(Some(paid(&handoff)))
            }
            None => Ok(None),
        }
    }

    /// The outputs (address, sats) of `txid` and the time the node first saw it, in ms.
    async fn transaction(
        &self,
        txid: &str,
    ) -> Result<Option<(Vec<(String, u64)>, u64)>, RailError> {
        let unavailable = |error: RpcError| RailError::Unavailable(error.to_string());
        let in_wallet = self
            .node
            .call(
                Some(&self.wallet),
                "gettransaction",
                json!({ "txid": txid, "include_watchonly": true, "verbose": true }),
            )
            .await;
        let (vout, seen_s) = match in_wallet {
            Ok(tx) => {
                let seen = tx
                    .get("timereceived")
                    .or_else(|| tx.get("time"))
                    .and_then(Value::as_u64);
                (tx.pointer("/decoded/vout").cloned(), seen)
            }
            Err(RpcError::Rpc { code, .. }) if code == INVALID_ADDRESS_OR_KEY => {
                match self
                    .node
                    .call(None, "getrawtransaction", json!([txid, true]))
                    .await
                {
                    Ok(tx) => {
                        let seen = match tx.get("blocktime").and_then(Value::as_u64) {
                            Some(time) => Some(time),
                            None => self
                                .node
                                .call(None, "getmempoolentry", json!([txid]))
                                .await
                                .ok()
                                .and_then(|entry| entry.get("time").and_then(Value::as_u64)),
                        };
                        (tx.get("vout").cloned(), seen)
                    }
                    Err(RpcError::Rpc { code, .. }) if code == INVALID_ADDRESS_OR_KEY => {
                        return Ok(None)
                    }
                    Err(error) => return Err(unavailable(error)),
                }
            }
            Err(error) => return Err(unavailable(error)),
        };
        let outputs = vout
            .as_ref()
            .and_then(Value::as_array)
            .map(|outputs| {
                outputs
                    .iter()
                    .filter_map(|out| {
                        let address = out.pointer("/scriptPubKey/address")?.as_str()?;
                        let value = out.get("value")?.as_f64()?;
                        Some((address.to_owned(), btc_to_sats(value)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let seen_ms = seen_s.map_or_else(now_ms, |s| s.saturating_mul(1000));
        Ok(Some((outputs, seen_ms)))
    }

    async fn is_mine(&self, address: &str) -> Result<bool, RailError> {
        let info = self
            .node
            .call(Some(&self.wallet), "getaddressinfo", json!([address]))
            .await
            .map_err(|error| RailError::Unavailable(error.to_string()))?;
        Ok(info.get("ismine").and_then(Value::as_bool).unwrap_or(false))
    }

    async fn verify_inner(&self, check: ProofCheck) -> Result<Option<u64>, RailError> {
        if !PAYS.contains(&check.endpoint_id.as_str()) {
            return Ok(None);
        }
        let Some(txid) = proof_txid(&check.proof) else {
            return Ok(None);
        };
        let Some(want) = amount_sats(&check.amount) else {
            return Ok(None);
        };
        let target = check.payload.as_ref().and_then(payload_address);
        if check.payload.is_some() && target.is_none() {
            return Ok(None);
        }
        let Some((outputs, seen_ms)) = self.transaction(&txid).await? else {
            // Not known to the node yet: can't check now, so the host tries again later.
            return Err(RailError::Unavailable(
                "the transaction isn't known to the node yet".into(),
            ));
        };
        let mut received: u64 = 0;
        for (address, sats) in outputs {
            // Every output to the address counts: a batch transaction can pay it more than once.
            let counts = match &target {
                Some(target) => &address == target,
                None => self.is_mine(&address).await?,
            };
            if counts {
                received = received.saturating_add(sats);
            }
        }
        lock(&self.verified).insert(
            check.attempt_id.to_text(),
            Verified {
                txid,
                sats_received: received,
            },
        );
        Ok((received >= want).then_some(seen_ms))
    }
}

impl Rail for RegtestRail {
    fn declaration(&self) -> RailDeclaration {
        RailDeclaration::new(LONGEST_COMPLETION_MS, PAYMENT_TIME_TOLERANCE_MS)
            .with_idempotent(true)
            .with_history(true)
            .with_history_survives_restore(true)
    }

    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        _amount: Option<&PaymentAmount>,
    ) -> Result<EndpointSelection, RailError> {
        Ok(EndpointSelection::new(
            candidates
                .iter()
                .filter(|candidate| {
                    PAYS.contains(&candidate.endpoint_id.as_str())
                        && payload_address(&candidate.payload).is_some()
                })
                .map(|candidate| candidate.id)
                .collect(),
        ))
    }

    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        PAYS.iter()
            .filter_map(|id| PaymentEndpointIdentifier::new(id).ok())
            .collect()
    }

    fn transfer(
        &self,
        _endpoint_id: &PaymentEndpointIdentifier,
        _payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        // An address payload fixes no amount: exactly the amount asked is sent.
        Transfer::new(Some(amount.clone()))
    }

    fn fraction_digits(&self, endpoint_id: &PaymentEndpointIdentifier) -> Option<u32> {
        PAYS.contains(&endpoint_id.as_str())
            .then_some(FRACTION_DIGITS)
    }

    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        Box::pin(self.pay_inner(payment))
    }

    fn payment(
        &self,
        idempotency_key: AttemptId,
    ) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>> {
        Box::pin(self.payment_inner(idempotency_key))
    }

    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        Box::pin(self.verify_inner(check))
    }

    fn settlement_id(
        &self,
        endpoint_id: &PaymentEndpointIdentifier,
        proof: &Map<String, Value>,
    ) -> Option<Vec<u8>> {
        if !PAYS.contains(&endpoint_id.as_str()) {
            return None;
        }
        unhex::<32>(&proof_txid(proof)?).map(|bytes| bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(value: &str, asset: &str) -> PaymentAmount {
        PaymentAmount {
            value: paykit_lib::Decimal::new(value).unwrap(),
            asset: paykit_lib::Asset::new(asset).unwrap(),
        }
    }

    #[test]
    fn test_rail_amounts_are_whole_satoshis() {
        assert_eq!(amount_sats(&amount("0.0001", "btc")), Some(10_000));
        assert_eq!(amount_sats(&btc_amount(10_000).unwrap()), Some(10_000));
        assert_eq!(
            btc_amount(123_456_789).unwrap().value.to_string(),
            "1.23456789"
        );
        // A fraction of a satoshi is rounded up, so the payer never sends less.
        assert_eq!(amount_sats(&amount("0.000000011", "btc")), Some(2));
        assert_eq!(amount_sats(&amount("1", "usd")), None);
    }

    #[test]
    fn test_rail_proof_and_settlement_id() {
        let txid = "ab".repeat(32);
        let proof = json!({ "txid": txid }).as_object().cloned().unwrap();
        assert_eq!(proof_txid(&proof).as_deref(), Some(txid.as_str()));
        let id = PaymentEndpointIdentifier::new(P2WPKH).unwrap();
        let rail = RegtestRail::new(
            Bitcoind::new("http://127.0.0.1:18443", "u", "p").unwrap(),
            "w".into(),
            None,
        )
        .unwrap();
        assert_eq!(rail.settlement_id(&id, &proof), Some(vec![0xab; 32]));
        for bad in [
            json!({ "txid": "AB".repeat(32) }),
            json!({ "txid": "ab".repeat(31) }),
            json!({ "txid": 7 }),
            json!({}),
        ] {
            let proof = bad.as_object().cloned().unwrap();
            assert_eq!(proof_txid(&proof), None, "{bad}");
            assert_eq!(rail.settlement_id(&id, &proof), None, "{bad}");
        }
        let other = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let proof = json!({ "txid": txid }).as_object().cloned().unwrap();
        assert_eq!(rail.settlement_id(&other, &proof), None);
    }

    #[test]
    fn test_rail_payload_names_an_address() {
        let payload = address_payload("bcrt1qexample").unwrap();
        assert_eq!(payload.as_str(), r#"{"value":"bcrt1qexample"}"#);
        assert_eq!(payload_address(&payload).as_deref(), Some("bcrt1qexample"));
        for bad in [r#"{"value":""}"#, r#"{"address":"x"}"#, "bcrt1q"] {
            let payload = PaymentEndpointPayload::new(bad).unwrap();
            assert_eq!(payload_address(&payload), None, "{bad}");
        }
    }
}
