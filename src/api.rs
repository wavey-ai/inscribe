use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::{Amount, Network, OutPoint, Transaction, Txid};
use serde::Deserialize;

const USER_AGENT: &str = concat!("wavey-inscribe/", env!("CARGO_PKG_VERSION"));

pub fn default_esplora_url(network: Network) -> Option<&'static str> {
  match network {
    Network::Bitcoin => Some("https://mempool.space/api"),
    Network::Signet => Some("https://mempool.space/signet/api"),
    Network::Testnet4 => Some("https://mempool.space/testnet4/api"),
    _ => None,
  }
}

pub fn default_ord_url(network: Network) -> Option<&'static str> {
  match network {
    Network::Bitcoin => Some("https://ordinals.com"),
    _ => None,
  }
}

fn client() -> Result<reqwest::blocking::Client> {
  Ok(
    reqwest::blocking::Client::builder()
      .user_agent(USER_AGENT)
      .timeout(Duration::from_secs(60))
      .build()?,
  )
}

/// An Esplora REST endpoint, for example mempool.space.
pub struct Esplora {
  base: String,
  client: reqwest::blocking::Client,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Utxo {
  pub txid: Txid,
  pub vout: u32,
  pub value: u64,
  pub status: TxStatus,
}

impl Utxo {
  pub fn outpoint(&self) -> OutPoint {
    OutPoint::new(self.txid, self.vout)
  }

  pub fn amount(&self) -> Amount {
    Amount::from_sat(self.value)
  }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TxStatus {
  pub confirmed: bool,
  pub block_height: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Outspend {
  pub spent: bool,
  pub txid: Option<Txid>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendedFees {
  pub fastest_fee: f64,
  pub half_hour_fee: f64,
  pub hour_fee: f64,
  pub economy_fee: f64,
  pub minimum_fee: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Prices {
  #[serde(rename = "USD")]
  pub usd: f64,
  #[serde(rename = "GBP")]
  pub gbp: f64,
}

impl Esplora {
  pub fn new(base: &str) -> Result<Self> {
    Ok(Self {
      base: base.trim_end_matches('/').to_string(),
      client: client()?,
    })
  }

  fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
    let url = format!("{}{path}", self.base);
    let response = self
      .client
      .get(&url)
      .send()
      .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
      bail!(
        "GET {url}: HTTP {status}: {}",
        response.text().unwrap_or_default()
      );
    }
    response.json().with_context(|| format!("decode {url}"))
  }

  pub fn utxos(&self, address: &bitcoin::Address) -> Result<Vec<Utxo>> {
    self.get(&format!("/address/{address}/utxo"))
  }

  pub fn recommended_fees(&self) -> Result<RecommendedFees> {
    self.get("/v1/fees/recommended")
  }

  /// Exchange rates. mempool.space publishes these for mainnet only.
  pub fn prices(&self) -> Result<Prices> {
    self.get("/v1/prices")
  }

  /// Returns None when the endpoint does not know the transaction.
  pub fn tx_status(&self, txid: &Txid) -> Result<Option<TxStatus>> {
    let url = format!("{}/tx/{txid}/status", self.base);
    let response = self
      .client
      .get(&url)
      .send()
      .with_context(|| format!("GET {url}"))?;
    match response.status() {
      status if status == reqwest::StatusCode::NOT_FOUND => Ok(None),
      status if status.is_success() => Ok(Some(response.json()?)),
      status => bail!(
        "GET {url}: HTTP {status}: {}",
        response.text().unwrap_or_default()
      ),
    }
  }

  pub fn outspend(&self, outpoint: &OutPoint) -> Result<Outspend> {
    self.get(&format!("/tx/{}/outspend/{}", outpoint.txid, outpoint.vout))
  }

  pub fn broadcast(&self, tx: &Transaction) -> Result<Txid> {
    let url = format!("{}/tx", self.base);
    let hex = bitcoin::consensus::encode::serialize_hex(tx);
    let response = self
      .client
      .post(&url)
      .body(hex)
      .send()
      .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
      bail!(
        "broadcast of {} failed: HTTP {status}: {text}",
        tx.compute_txid()
      );
    }
    text
      .trim()
      .parse()
      .map_err(|err| anyhow!("broadcast returned {text:?}: {err}"))
  }
}

/// An ord server. The client uses the recursive endpoint /r/utxo, which
/// ordinals.com serves when its JSON API is off.
pub struct Ord {
  base: String,
  client: reqwest::blocking::Client,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OrdUtxo {
  #[serde(default)]
  pub inscriptions: Option<Vec<String>>,
  #[serde(default)]
  pub runes: Option<serde_json::Value>,
}

impl OrdUtxo {
  pub fn is_plain(&self) -> bool {
    let no_inscriptions = self
      .inscriptions
      .as_ref()
      .is_none_or(|list| list.is_empty());
    let no_runes = match &self.runes {
      None | Some(serde_json::Value::Null) => true,
      Some(serde_json::Value::Object(map)) => map.is_empty(),
      Some(serde_json::Value::Array(list)) => list.is_empty(),
      Some(_) => false,
    };
    no_inscriptions && no_runes
  }
}

impl Ord {
  pub fn new(base: &str) -> Result<Self> {
    Ok(Self {
      base: base.trim_end_matches('/').to_string(),
      client: client()?,
    })
  }

  /// Returns None when the ord server has not indexed the output.
  pub fn utxo(&self, outpoint: &OutPoint) -> Result<Option<OrdUtxo>> {
    let url = format!("{}/r/utxo/{outpoint}", self.base);
    let response = self
      .client
      .get(&url)
      .send()
      .with_context(|| format!("GET {url}"))?;
    match response.status() {
      status if status == reqwest::StatusCode::NOT_FOUND => Ok(None),
      status if status.is_success() => Ok(Some(
        response.json().with_context(|| format!("decode {url}"))?,
      )),
      status => bail!(
        "GET {url}: HTTP {status}: {}",
        response.text().unwrap_or_default()
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ord_utxo_with_inscription_is_not_plain() {
    let used: OrdUtxo = serde_json::from_str(
      r#"{"inscriptions":["16695e97fc2425f4ef6d23cb4aafb473c9ebd699a50adea94624a6cd15ba29e1i0"],"runes":{},"sat_ranges":[[1,2]],"value":546}"#,
    )
    .unwrap();
    assert!(!used.is_plain());

    let plain: OrdUtxo =
      serde_json::from_str(r#"{"inscriptions":[],"runes":{},"sat_ranges":null,"value":5000}"#)
        .unwrap();
    assert!(plain.is_plain());

    let runic: OrdUtxo =
      serde_json::from_str(r#"{"inscriptions":[],"runes":{"UNCOMMON•GOODS":{"amount":1}}}"#)
        .unwrap();
    assert!(!runic.is_plain());
  }
}
