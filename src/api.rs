//! Chain data and broadcast through an Esplora endpoint (for example
//! mempool.space) and an ord server. The client is async and generic over an
//! [`Http`] backend, so the same code runs with reqwest on a host and with the
//! Fetch API on Cloudflare Workers.

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::{Address, Amount, Network, OutPoint, Transaction, Txid};
use serde::Deserialize;
use serde::de::DeserializeOwned;

pub const USER_AGENT: &str = concat!("wavey-inscribe/", env!("CARGO_PKG_VERSION"));

/// An HTTP response: the status code and the body.
pub struct Response {
  pub status: u16,
  pub body: Vec<u8>,
}

impl Response {
  pub fn is_success(&self) -> bool {
    (200..300).contains(&self.status)
  }

  pub fn text(&self) -> String {
    String::from_utf8_lossy(&self.body).into_owned()
  }
}

/// The HTTP operations that the client needs. A Workers backend implements
/// this trait with `worker::Fetch`.
#[allow(async_fn_in_trait)]
pub trait Http {
  async fn get(&self, url: &str) -> Result<Response>;
  async fn post(&self, url: &str, body: String) -> Result<Response>;
}

#[derive(Clone, Debug)]
pub struct Endpoints {
  /// The Esplora REST base URL, for example https://mempool.space/api.
  pub esplora: String,
  /// The ord server base URL, for example https://ordinals.com.
  pub ord: Option<String>,
}

impl Endpoints {
  /// mempool.space for each public network, and ordinals.com on mainnet.
  pub fn defaults(network: Network) -> Option<Self> {
    let esplora = match network {
      Network::Bitcoin => "https://mempool.space/api",
      Network::Signet => "https://mempool.space/signet/api",
      Network::Testnet4 => "https://mempool.space/testnet4/api",
      _ => return None,
    };
    Some(Self {
      esplora: esplora.into(),
      ord: (network == Network::Bitcoin).then(|| "https://ordinals.com".into()),
    })
  }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
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

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct TxStatus {
  pub confirmed: bool,
  pub block_height: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Outspend {
  pub spent: bool,
  pub txid: Option<Txid>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendedFees {
  pub fastest_fee: f64,
  pub half_hour_fee: f64,
  pub hour_fee: f64,
  pub economy_fee: f64,
  pub minimum_fee: f64,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Prices {
  #[serde(rename = "USD")]
  pub usd: f64,
  #[serde(rename = "GBP")]
  pub gbp: f64,
}

/// The answer of the ord recursive endpoint /r/utxo/<outpoint>.
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

pub struct Client<H> {
  http: H,
  endpoints: Endpoints,
}

impl<H: Http> Client<H> {
  pub fn new(http: H, endpoints: Endpoints) -> Self {
    let trim = |url: &str| url.trim_end_matches('/').to_string();
    Self {
      http,
      endpoints: Endpoints {
        esplora: trim(&endpoints.esplora),
        ord: endpoints.ord.as_deref().map(trim),
      },
    }
  }

  pub fn http(&self) -> &H {
    &self.http
  }

  pub fn endpoints(&self) -> &Endpoints {
    &self.endpoints
  }

  async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
    let response = self
      .http
      .get(url)
      .await
      .with_context(|| format!("GET {url}"))?;
    if !response.is_success() {
      bail!("GET {url}: HTTP {}: {}", response.status, response.text());
    }
    serde_json::from_slice(&response.body).with_context(|| format!("decode {url}"))
  }

  /// GET that maps HTTP 404 to None.
  async fn get_optional<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
    let response = self
      .http
      .get(url)
      .await
      .with_context(|| format!("GET {url}"))?;
    match response.status {
      404 => Ok(None),
      _ if response.is_success() => Ok(Some(
        serde_json::from_slice(&response.body).with_context(|| format!("decode {url}"))?,
      )),
      status => bail!("GET {url}: HTTP {status}: {}", response.text()),
    }
  }

  pub async fn utxos(&self, address: &Address) -> Result<Vec<Utxo>> {
    self
      .get_json(&format!(
        "{}/address/{address}/utxo",
        self.endpoints.esplora
      ))
      .await
  }

  pub async fn recommended_fees(&self) -> Result<RecommendedFees> {
    self
      .get_json(&format!("{}/v1/fees/recommended", self.endpoints.esplora))
      .await
  }

  /// Exchange rates. mempool.space publishes these for mainnet only.
  pub async fn prices(&self) -> Result<Prices> {
    self
      .get_json(&format!("{}/v1/prices", self.endpoints.esplora))
      .await
  }

  /// Returns None when the endpoint does not know the transaction.
  pub async fn tx_status(&self, txid: &Txid) -> Result<Option<TxStatus>> {
    self
      .get_optional(&format!("{}/tx/{txid}/status", self.endpoints.esplora))
      .await
  }

  pub async fn outspend(&self, outpoint: &OutPoint) -> Result<Outspend> {
    self
      .get_json(&format!(
        "{}/tx/{}/outspend/{}",
        self.endpoints.esplora, outpoint.txid, outpoint.vout
      ))
      .await
  }

  pub async fn broadcast(&self, tx: &Transaction) -> Result<Txid> {
    let url = format!("{}/tx", self.endpoints.esplora);
    let response = self
      .http
      .post(&url, bitcoin::consensus::encode::serialize_hex(tx))
      .await
      .with_context(|| format!("POST {url}"))?;
    let text = response.text();
    if !response.is_success() {
      bail!(
        "broadcast of {} failed: HTTP {}: {text}",
        tx.compute_txid(),
        response.status
      );
    }
    text
      .trim()
      .parse()
      .map_err(|err| anyhow!("broadcast returned {text:?}: {err}"))
  }

  pub fn has_ord(&self) -> bool {
    self.endpoints.ord.is_some()
  }

  /// Returns None when the ord server has not indexed the output. The client
  /// uses the recursive endpoint, which ordinals.com serves when its JSON API
  /// is off.
  pub async fn ord_utxo(&self, outpoint: &OutPoint) -> Result<Option<OrdUtxo>> {
    let ord = self
      .endpoints
      .ord
      .as_deref()
      .ok_or_else(|| anyhow!("no ord server is set"))?;
    self.get_optional(&format!("{ord}/r/utxo/{outpoint}")).await
  }
}

/// A blocking reqwest backend for native programs. Its futures complete
/// without suspension, so a minimal executor such as `pollster` runs them.
#[cfg(feature = "native")]
pub struct ReqwestHttp {
  client: reqwest::blocking::Client,
}

#[cfg(feature = "native")]
impl ReqwestHttp {
  pub fn new() -> Result<Self> {
    Ok(Self {
      client: reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(60))
        .build()?,
    })
  }

  fn finish(response: reqwest::blocking::Response) -> Result<Response> {
    Ok(Response {
      status: response.status().as_u16(),
      body: response.bytes()?.to_vec(),
    })
  }
}

#[cfg(feature = "native")]
impl Http for ReqwestHttp {
  async fn get(&self, url: &str) -> Result<Response> {
    Self::finish(self.client.get(url).send()?)
  }

  async fn post(&self, url: &str, body: String) -> Result<Response> {
    Self::finish(self.client.post(url).body(body).send()?)
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
