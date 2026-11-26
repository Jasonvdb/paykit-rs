//! A minimal bitcoind JSON-RPC client, for the local regtest node only (loopback, demo
//! credentials).

use std::time::Duration;

use serde_json::{json, Value};

/// Why a call failed.
#[derive(Debug)]
pub enum RpcError {
    /// The request never reached bitcoind: nothing was done.
    NotSent(String),
    /// The request may have reached bitcoind, and its answer was lost.
    Unknown(String),
    /// bitcoind answered with an error.
    Rpc { code: i64, message: String },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(why) => write!(f, "bitcoind not reached: {why}"),
            Self::Unknown(why) => write!(f, "bitcoind answer lost: {why}"),
            Self::Rpc { code, message } => write!(f, "bitcoind error {code}: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// `RPC_WALLET_NOT_FOUND`.
const WALLET_NOT_FOUND: i64 = -18;
/// `RPC_WALLET_ALREADY_LOADED`.
const WALLET_ALREADY_LOADED: i64 = -35;
/// `RPC_INVALID_ADDRESS_OR_KEY`: also "no such wallet transaction".
pub const INVALID_ADDRESS_OR_KEY: i64 = -5;

#[derive(Clone)]
pub struct Bitcoind {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

impl Bitcoind {
    /// `url` must be `http://127.0.0.1:<port>` or `http://localhost:<port>`.
    pub fn new(url: &str, user: &str, password: &str) -> anyhow::Result<Self> {
        crate::transport::check_loopback_url("--bitcoind-url", url)?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: url.trim_end_matches('/').to_owned(),
            user: user.to_owned(),
            password: password.to_owned(),
        })
    }

    /// Call `method` with `params` (an array, or an object of named parameters), on `wallet`
    /// when given.
    pub async fn call(
        &self,
        wallet: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let url = match wallet {
            Some(wallet) => format!("{}/wallet/{wallet}", self.url),
            None => self.url.clone(),
        };
        let body =
            json!({ "jsonrpc": "1.0", "id": "paykit-peer", "method": method, "params": params });
        let answer = self
            .http
            .post(url)
            .basic_auth(&self.user, Some(&self.password))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() {
                    RpcError::NotSent(error.to_string())
                } else {
                    RpcError::Unknown(error.to_string())
                }
            })?;
        let status = answer.status();
        let bytes = answer
            .bytes()
            .await
            .map_err(|error| RpcError::Unknown(error.to_string()))?;
        let reply: Value = serde_json::from_slice(&bytes).map_err(|_| {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                RpcError::NotSent(format!("bitcoind refused the credentials ({status})"))
            } else {
                RpcError::Unknown(format!("bitcoind answered {status} without JSON"))
            }
        })?;
        match reply.get("error") {
            Some(error) if !error.is_null() => Err(RpcError::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            _ => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    /// Load `wallet`, creating it when it doesn't exist.
    pub async fn ensure_wallet(&self, wallet: &str) -> Result<(), RpcError> {
        match self.call(None, "loadwallet", json!([wallet])).await {
            Ok(_) => Ok(()),
            Err(RpcError::Rpc { code, .. }) if code == WALLET_ALREADY_LOADED => Ok(()),
            Err(RpcError::Rpc { code, .. }) if code == WALLET_NOT_FOUND => {
                match self.call(None, "createwallet", json!([wallet])).await {
                    Ok(_) => Ok(()),
                    // Created by a racing call: load it.
                    Err(RpcError::Rpc { code, .. }) if code == WALLET_ALREADY_LOADED => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// A new bech32 (P2WPKH) receive address of `wallet`.
    pub async fn new_address(&self, wallet: &str) -> Result<String, RpcError> {
        self.new_address_of_type(wallet, "bech32").await
    }

    /// A new bech32m (P2TR, Taproot) receive address of `wallet`.
    pub async fn new_taproot_address(&self, wallet: &str) -> Result<String, RpcError> {
        self.new_address_of_type(wallet, "bech32m").await
    }

    async fn new_address_of_type(
        &self,
        wallet: &str,
        address_type: &str,
    ) -> Result<String, RpcError> {
        let address = self
            .call(Some(wallet), "getnewaddress", json!(["", address_type]))
            .await?;
        address
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| RpcError::Unknown("getnewaddress returned no address".into()))
    }

    /// The trusted balance of `wallet`, in satoshis.
    pub async fn balance_sats(&self, wallet: &str) -> Result<u64, RpcError> {
        let balance = self.call(Some(wallet), "getbalance", json!([])).await?;
        balance
            .as_f64()
            .map(btc_to_sats)
            .ok_or_else(|| RpcError::Unknown("getbalance returned no number".into()))
    }

    /// Mine `blocks` blocks to an address of `wallet`.
    pub async fn mine(&self, wallet: &str, blocks: u32) -> Result<(), RpcError> {
        let address = self.new_address(wallet).await?;
        self.call(None, "generatetoaddress", json!([blocks, address]))
            .await?;
        Ok(())
    }

    /// Fund `wallet` with at least `sats` from `funder`, mining what the funder lacks, then
    /// mine one block so the funds are confirmed. Returns the funding txid, or `None` when the
    /// wallet already held enough.
    pub async fn fund(
        &self,
        wallet: &str,
        funder: &str,
        sats: u64,
    ) -> Result<Option<String>, RpcError> {
        self.ensure_wallet(wallet).await?;
        if self.balance_sats(wallet).await? >= sats {
            return Ok(None);
        }
        self.ensure_wallet(funder).await?;
        // A coinbase matures after 100 blocks.
        if self.balance_sats(funder).await? < sats.saturating_add(100_000) {
            self.mine(funder, 101).await?;
        }
        let address = self.new_address(wallet).await?;
        let txid = self
            .call(
                Some(funder),
                "sendtoaddress",
                json!({ "address": address, "amount": sats_to_btc(sats), "fee_rate": 2 }),
            )
            .await?;
        self.mine(funder, 1).await?;
        Ok(txid.as_str().map(str::to_owned))
    }
}

/// Satoshis of a BTC amount bitcoind reported as a JSON number.
pub fn btc_to_sats(btc: f64) -> u64 {
    // Amounts below 21e6 BTC have at most 16 significant digits in satoshis, which f64 holds;
    // rounding removes the binary representation error.
    let sats = (btc * 100_000_000.0).round();
    if sats <= 0.0 {
        0
    } else {
        sats as u64
    }
}

/// The decimal BTC text of `sats`, with 8 fraction digits.
pub fn sats_to_btc(sats: u64) -> String {
    format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}
