use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;
use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::{Amount, Network, Transaction};

use inscribe::api::{Client, Endpoints, Http, Response};
use inscribe::service::{self, CreateOptions, FileInput, SkipReason, TxState};
use inscribe::wallet::Wallet;

const MNEMONIC: &str =
  "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const ESPLORA: &str = "https://esplora.test/api";
const ORD: &str = "https://ord.test";

/// An in-memory Esplora and ord server. Broadcast transactions become known
/// to the status endpoint.
#[derive(Default)]
struct MockHttp {
  routes: RefCell<HashMap<String, (u16, String)>>,
  broadcasts: RefCell<Vec<Transaction>>,
  reject_broadcast_after: Option<usize>,
}

impl MockHttp {
  fn route(&self, path: &str, status: u16, body: &str) {
    self
      .routes
      .borrow_mut()
      .insert(path.to_string(), (status, body.to_string()));
  }
}

impl Http for MockHttp {
  async fn get(&self, url: &str) -> Result<Response> {
    // mempool.space answers /tx/<txid>/status with HTTP 200 for every txid,
    // also for a transaction that it has never seen.
    if url.ends_with("/status") && !self.routes.borrow().contains_key(url) {
      return Ok(Response {
        status: 200,
        body: br#"{"confirmed":false}"#.to_vec(),
      });
    }
    let (status, body) = self
      .routes
      .borrow()
      .get(url)
      .cloned()
      .unwrap_or((404, "not found".into()));
    Ok(Response {
      status,
      body: body.into_bytes(),
    })
  }

  async fn post(&self, url: &str, body: String) -> Result<Response> {
    assert_eq!(url, format!("{ESPLORA}/tx"));
    if let Some(limit) = self.reject_broadcast_after {
      if self.broadcasts.borrow().len() >= limit {
        return Ok(Response {
          status: 400,
          body: b"sendrawtransaction RPC error: min relay fee not met".to_vec(),
        });
      }
    }
    let tx: Transaction = deserialize_hex(&body)?;
    let txid = tx.compute_txid();
    self.route(
      &format!("{ESPLORA}/tx/{txid}"),
      200,
      &format!(r#"{{"txid":"{txid}","status":{{"confirmed":false}}}}"#),
    );
    self.broadcasts.borrow_mut().push(tx);
    Ok(Response {
      status: 200,
      body: txid.to_string().into_bytes(),
    })
  }
}

fn setup(reject_broadcast_after: Option<usize>) -> (Client<MockHttp>, Wallet) {
  let wallet = Wallet::from_phrase(MNEMONIC, Network::Bitcoin).unwrap();
  let http = MockHttp {
    reject_broadcast_after,
    ..MockHttp::default()
  };
  let address = wallet.address();
  let plain = "1111111111111111111111111111111111111111111111111111111111111111";
  let inscribed = "2222222222222222222222222222222222222222222222222222222222222222";
  let pending = "3333333333333333333333333333333333333333333333333333333333333333";
  http.route(
    &format!("{ESPLORA}/address/{address}/utxo"),
    200,
    &format!(
      r#"[{{"txid":"{plain}","vout":0,"value":500000,"status":{{"confirmed":true,"block_height":900000}}}},
          {{"txid":"{inscribed}","vout":1,"value":546,"status":{{"confirmed":true,"block_height":900001}}}},
          {{"txid":"{pending}","vout":0,"value":80000,"status":{{"confirmed":false}}}}]"#
    ),
  );
  http.route(
    &format!("{ESPLORA}/v1/fees/recommended"),
    200,
    r#"{"fastestFee":3,"halfHourFee":2,"hourFee":1,"economyFee":1,"minimumFee":1}"#,
  );
  http.route(
    &format!("{ORD}/r/utxo/{plain}:0"),
    200,
    r#"{"inscriptions":[],"runes":{},"sat_ranges":null,"value":500000}"#,
  );
  http.route(
    &format!("{ORD}/r/utxo/{inscribed}:1"),
    200,
    r#"{"inscriptions":["aai0"],"runes":{},"value":546}"#,
  );
  let client = Client::new(
    http,
    Endpoints {
      esplora: ESPLORA.into(),
      ord: Some(ORD.into()),
    },
  );
  (client, wallet)
}

fn files() -> Vec<FileInput> {
  vec![
    FileInput {
      name: "cover.png".into(),
      bytes: vec![0x89; 3000],
      content_type: None,
    },
    FileInput {
      name: "player.html".into(),
      bytes: b"<p>play</p>".repeat(300),
      content_type: None,
    },
  ]
}

fn options(wallet: &Wallet) -> CreateOptions {
  CreateOptions {
    destination: wallet.address(),
    postage: Amount::from_sat(546),
    reveal_fee_rate: None,
    commit_fee_rate: None,
    include_unconfirmed: false,
    ord_check: true,
  }
}

#[test]
fn create_skips_unsafe_outputs_and_uses_the_half_hour_rate() {
  let (client, wallet) = setup(None);
  let prepared = service::prepare(files(), true, None).unwrap();
  let created = pollster::block_on(service::create(
    &client,
    &wallet,
    prepared,
    &options(&wallet),
    1_791_800_000,
    &mut rand_core::OsRng,
  ))
  .unwrap();

  let reasons: Vec<SkipReason> = created.skipped.iter().map(|skip| skip.reason).collect();
  assert_eq!(
    reasons,
    vec![SkipReason::HoldsInscriptionOrRunes, SkipReason::Unconfirmed]
  );
  assert_eq!(created.run.reveal_fee_rate, 2.0);
  assert_eq!(created.run.created_unix, 1_791_800_000);
  assert_eq!(created.run.reveals[0].content_type, "image/png");
  assert_eq!(
    created.run.reveals[1].content_type,
    "text/html;charset=utf-8"
  );
  assert_eq!(
    created.run.reveals[1].content_encoding.as_deref(),
    Some("br")
  );
  // Only the plain, confirmed output funds the commit.
  let commit: Transaction = created.run.commit.transaction().unwrap();
  assert_eq!(commit.input.len(), 1);
  assert_eq!(
    commit.input[0].previous_output.txid.to_string(),
    "1111111111111111111111111111111111111111111111111111111111111111"
  );
  // A copy for API responses carries no keys.
  assert!(
    created
      .run
      .without_secrets()
      .reveals
      .iter()
      .all(|reveal| reveal.secret_key.is_empty())
  );
}

#[test]
fn broadcast_sends_commit_then_reveals_and_is_idempotent() {
  let (client, wallet) = setup(None);
  let prepared = service::prepare(files(), false, None).unwrap();
  let mut run = pollster::block_on(service::create(
    &client,
    &wallet,
    prepared,
    &options(&wallet),
    1,
    &mut rand_core::OsRng,
  ))
  .unwrap()
  .run;

  let report = pollster::block_on(service::broadcast(&client, &mut run)).unwrap();
  assert!(report.failures.is_empty());
  assert_eq!(report.revealed.len(), 2);
  assert!(run.commit.broadcast && run.reveals.iter().all(|reveal| reveal.tx.broadcast));

  let sent: Vec<String> = client_broadcasts(&client);
  assert_eq!(
    sent,
    vec![
      run.commit.txid.clone(),
      run.reveals[0].tx.txid.clone(),
      run.reveals[1].tx.txid.clone()
    ]
  );

  // A second call finds every transaction and sends nothing.
  pollster::block_on(service::broadcast(&client, &mut run)).unwrap();
  assert_eq!(client_broadcasts(&client).len(), 3);

  // A flag that says "broadcast" does not stop a resend of a transaction that
  // the network does not have, for example after a mempool eviction.
  let evicted = run.reveals[1].tx.txid.clone();
  http(&client)
    .routes
    .borrow_mut()
    .remove(&format!("{ESPLORA}/tx/{evicted}"));
  assert!(run.reveals[1].tx.broadcast);
  pollster::block_on(service::broadcast(&client, &mut run)).unwrap();
  assert_eq!(client_broadcasts(&client).len(), 4);
  assert_eq!(client_broadcasts(&client)[3], evicted);

  let status = pollster::block_on(service::status(&client, &run)).unwrap();
  assert_eq!(status.commit, TxState::Mempool);
  assert!(
    status
      .reveals
      .iter()
      .all(|reveal| reveal.state == TxState::Mempool)
  );
}

#[test]
fn failed_reveals_are_reported_and_recoverable() {
  // The mock accepts the commit and the first reveal, then rejects.
  let (client, wallet) = setup(Some(2));
  let prepared = service::prepare(files(), false, None).unwrap();
  let mut run = pollster::block_on(service::create(
    &client,
    &wallet,
    prepared,
    &options(&wallet),
    1,
    &mut rand_core::OsRng,
  ))
  .unwrap()
  .run;

  let report = pollster::block_on(service::broadcast(&client, &mut run)).unwrap();
  assert_eq!(report.revealed.len(), 1);
  assert_eq!(report.failures.len(), 1);
  assert!(report.failures[0].error.contains("min relay fee"));

  // The second commit output is unspent and its reveal is unknown.
  let commit_txid = &run.commit.txid;
  client_route(
    &client,
    &format!("{ESPLORA}/tx/{commit_txid}/outspend/1"),
    r#"{"spent":false}"#,
  );
  client_route(
    &client,
    &format!("{ESPLORA}/tx/{commit_txid}/outspend/0"),
    r#"{"spent":true}"#,
  );
  let recovery = pollster::block_on(service::recovery(
    &client,
    &wallet,
    &run,
    None,
    &mut rand_core::OsRng,
  ))
  .unwrap()
  .expect("one output to recover");
  assert_eq!(recovery.outputs.len(), 1);
  assert_eq!(recovery.outputs[0].vout, 1);
  assert_eq!(
    recovery.tx.output[0].script_pubkey,
    wallet.address().script_pubkey()
  );
}

fn client_broadcasts(client: &Client<MockHttp>) -> Vec<String> {
  http(client)
    .broadcasts
    .borrow()
    .iter()
    .map(|tx| tx.compute_txid().to_string())
    .collect()
}

fn client_route(client: &Client<MockHttp>, path: &str, body: &str) {
  http(client).route(path, 200, body);
}

fn http(client: &Client<MockHttp>) -> &MockHttp {
  client.http()
}
