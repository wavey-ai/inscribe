//! An authenticated JSON API over the inscribe library.
//!
//! Bindings:
//! - `INSCRIBE_MNEMONIC` (secret): the BIP39 mnemonic of the funding wallet.
//! - `INSCRIBE_API_TOKEN` (secret): the bearer token for every request.
//! - `INSCRIBE_NETWORK` (var): `bitcoin`, `signet`, or `testnet4`.
//! - `INSCRIBE_ESPLORA_URL`, `INSCRIBE_ORD_URL` (optional vars): endpoint overrides.
//! - `INSCRIBE_RUNS` (KV namespace): the run records. A record holds the
//!   commit-output keys, so the Worker writes it before any broadcast.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use inscribe::api::{Client, Endpoints, Http};
use inscribe::bitcoin::{Address, Amount, Network};
use inscribe::plan::{self, FeeRate};
use inscribe::run::Run;
use inscribe::service::{self, CreateOptions, FileInput};
use inscribe::wallet::Wallet;
use rand_core::OsRng;
use serde::Deserialize;
use serde_json::json;
use worker::{
  Context, Date, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result, RouteContext,
  Router, event,
};

const RUNS: &str = "INSCRIBE_RUNS";

/// The workers-rs Fetch API as an inscribe HTTP backend.
struct FetchHttp;

impl FetchHttp {
  async fn send(request: Request) -> anyhow::Result<inscribe::Response> {
    let mut response = Fetch::Request(request)
      .send()
      .await
      .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(inscribe::Response {
      status: response.status_code(),
      body: response
        .bytes()
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?,
    })
  }

  fn request(url: &str, method: Method, body: Option<String>) -> anyhow::Result<Request> {
    let headers = Headers::new();
    headers
      .set("user-agent", inscribe::api::USER_AGENT)
      .map_err(|err| anyhow::anyhow!("{err}"))?;
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(body) = body {
      init.with_body(Some(body.into()));
    }
    Request::new_with_init(url, &init).map_err(|err| anyhow::anyhow!("{err}"))
  }
}

impl Http for FetchHttp {
  async fn get(&self, url: &str) -> anyhow::Result<inscribe::Response> {
    Self::send(Self::request(url, Method::Get, None)?).await
  }

  async fn post(&self, url: &str, body: String) -> anyhow::Result<inscribe::Response> {
    Self::send(Self::request(url, Method::Post, Some(body))?).await
  }
}

struct State {
  network: Network,
  client: Client<FetchHttp>,
}

impl State {
  fn new(env: &Env) -> Result<Self> {
    let network = match var(env, "INSCRIBE_NETWORK").as_deref() {
      Some("bitcoin") | None => Network::Bitcoin,
      Some("signet") => Network::Signet,
      Some("testnet4") => Network::Testnet4,
      Some(other) => return Err(format!("unknown INSCRIBE_NETWORK {other}").into()),
    };
    let defaults = Endpoints::defaults(network).ok_or("no default endpoints for the network")?;
    let endpoints = Endpoints {
      esplora: var(env, "INSCRIBE_ESPLORA_URL").unwrap_or(defaults.esplora),
      ord: var(env, "INSCRIBE_ORD_URL").or(defaults.ord),
    };
    Ok(Self {
      network,
      client: Client::new(FetchHttp, endpoints),
    })
  }

  fn wallet(&self, env: &Env) -> Result<Wallet> {
    let phrase = env.secret("INSCRIBE_MNEMONIC")?.to_string();
    Wallet::from_phrase(&phrase, self.network).map_err(to_worker)
  }
}

fn var(env: &Env, name: &str) -> Option<String> {
  env
    .var(name)
    .ok()
    .map(|value| value.to_string())
    .filter(|value| !value.is_empty())
}

fn to_worker(err: anyhow::Error) -> worker::Error {
  worker::Error::RustError(format!("{err:#}"))
}

fn error(status: u16, message: impl std::fmt::Display) -> Result<Response> {
  Ok(Response::from_json(&json!({ "error": message.to_string() }))?.with_status(status))
}

/// Compares the bearer token in constant time.
fn authorized(request: &Request, env: &Env) -> Result<bool> {
  let expected = env.secret("INSCRIBE_API_TOKEN")?.to_string();
  let supplied = request
    .headers()
    .get("authorization")?
    .and_then(|value| value.strip_prefix("Bearer ").map(str::to_string))
    .unwrap_or_default();
  let (a, b) = (expected.as_bytes(), supplied.as_bytes());
  let mut difference = (a.len() ^ b.len()) as u8;
  for index in 0..a.len().max(b.len()) {
    difference |= a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0);
  }
  Ok(!expected.is_empty() && difference == 0)
}

#[derive(Deserialize)]
struct FileBody {
  name: String,
  /// The file content in standard base64.
  body_base64: String,
  content_type: Option<String>,
}

#[derive(Deserialize)]
struct ContentBody {
  files: Vec<FileBody>,
  #[serde(default)]
  compress: bool,
  metaprotocol: Option<String>,
  postage: Option<u64>,
  fee_rate: Option<f64>,
  commit_fee_rate: Option<f64>,
}

impl ContentBody {
  fn prepare(
    self,
  ) -> anyhow::Result<(service::Prepared, Amount, Option<FeeRate>, Option<FeeRate>)> {
    let files = self
      .files
      .into_iter()
      .map(|file| {
        Ok(FileInput {
          bytes: BASE64
            .decode(file.body_base64.as_bytes())
            .map_err(|err| anyhow::anyhow!("{}: {err}", file.name))?,
          name: file.name,
          content_type: file.content_type,
        })
      })
      .collect::<anyhow::Result<Vec<_>>>()?;
    let prepared = service::prepare(files, self.compress, self.metaprotocol.as_deref())?;
    Ok((
      prepared,
      Amount::from_sat(self.postage.unwrap_or(546)),
      self.fee_rate.map(FeeRate::new).transpose()?,
      self.commit_fee_rate.map(FeeRate::new).transpose()?,
    ))
  }
}

#[derive(Deserialize)]
struct CreateBody {
  #[serde(flatten)]
  content: ContentBody,
  destination: String,
  #[serde(default)]
  include_unconfirmed: bool,
  /// Broadcast after the run is stored. The default is false.
  #[serde(default)]
  broadcast: bool,
}

#[derive(Deserialize, Default)]
struct RecoverBody {
  fee_rate: Option<f64>,
}

async fn load_run(env: &Env, id: &str) -> Result<Option<Run>> {
  match env.kv(RUNS)?.get(&format!("run:{id}")).text().await? {
    Some(text) => Ok(Some(serde_json::from_str(&text)?)),
    None => Ok(None),
  }
}

async fn store_run(env: &Env, run: &Run) -> Result<()> {
  env
    .kv(RUNS)?
    .put(&format!("run:{}", run.id()), serde_json::to_string(run)?)?
    .execute()
    .await?;
  Ok(())
}

async fn wallet_info(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let wallet = state.wallet(&ctx.env)?;
  let utxos = state
    .client
    .utxos(&wallet.address())
    .await
    .map_err(to_worker)?;
  Response::from_json(&json!({
    "network": state.network.to_string(),
    "address": wallet.address().to_string(),
    "descriptor": wallet.descriptor(),
    "utxos": utxos,
  }))
}

async fn fees(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  Response::from_json(&state.client.recommended_fees().await.map_err(to_worker)?)
}

async fn estimate(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let body: ContentBody = match req.json().await {
    Ok(body) => body,
    Err(err) => return error(400, err),
  };
  let (prepared, postage, reveal, commit) = match body.prepare() {
    Ok(parts) => parts,
    Err(err) => return error(400, format!("{err:#}")),
  };
  let reveal = service::fee_rate(&state.client, reveal)
    .await
    .map_err(to_worker)?;
  let secp = inscribe::bitcoin::secp256k1::Secp256k1::new();
  match plan::estimate(
    &secp,
    prepared.inscriptions,
    postage,
    commit.unwrap_or(reveal),
    reveal,
  ) {
    Ok(estimate) => Response::from_json(&json!({
      "files": prepared.names,
      "reveal_fee_rate": reveal.sat_per_vb(),
      "estimate": estimate,
    })),
    Err(err) => error(400, format!("{err:#}")),
  }
}

async fn create_run(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let wallet = state.wallet(&ctx.env)?;
  let body: CreateBody = match req.json().await {
    Ok(body) => body,
    Err(err) => return error(400, err),
  };
  let destination = match body
    .destination
    .parse::<Address<_>>()
    .map_err(anyhow::Error::from)
    .and_then(|address| Ok(address.require_network(state.network)?))
  {
    Ok(address) => address,
    Err(err) => return error(400, format!("destination: {err:#}")),
  };
  let broadcast = body.broadcast;
  let (prepared, postage, reveal_fee_rate, commit_fee_rate) = match body.content.prepare() {
    Ok(parts) => parts,
    Err(err) => return error(400, format!("{err:#}")),
  };
  let options = CreateOptions {
    destination,
    postage,
    reveal_fee_rate,
    commit_fee_rate,
    include_unconfirmed: body.include_unconfirmed,
    ord_check: state.network == Network::Bitcoin,
  };
  let created = match service::create(
    &state.client,
    &wallet,
    prepared,
    &options,
    Date::now().as_millis() / 1000,
    &mut OsRng,
  )
  .await
  {
    Ok(created) => created,
    Err(err) => return error(422, format!("{err:#}")),
  };

  let mut run = created.run;
  store_run(&ctx.env, &run).await?;

  let report = if broadcast {
    let report = service::broadcast(&state.client, &mut run).await;
    store_run(&ctx.env, &run).await?;
    Some(report.map_err(to_worker)?)
  } else {
    None
  };

  Response::from_json(&json!({
    "id": run.id(),
    "run": run.without_secrets(),
    "skipped": created.skipped,
    "total_fee_sat": created.plan.total_spent().to_sat(),
    "broadcast": report,
  }))
}

async fn get_run(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let id = ctx.param("id").cloned().unwrap_or_default();
  let Some(run) = load_run(&ctx.env, &id).await? else {
    return error(404, "no run with this id");
  };
  let status = service::status(&state.client, &run)
    .await
    .map_err(to_worker)?;
  Response::from_json(&json!({ "id": id, "run": run.without_secrets(), "status": status }))
}

async fn broadcast_run(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let id = ctx.param("id").cloned().unwrap_or_default();
  let Some(mut run) = load_run(&ctx.env, &id).await? else {
    return error(404, "no run with this id");
  };
  let report = service::broadcast(&state.client, &mut run).await;
  store_run(&ctx.env, &run).await?;
  Response::from_json(&json!({ "id": id, "broadcast": report.map_err(to_worker)? }))
}

async fn recover_run(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
  let state = State::new(&ctx.env)?;
  let wallet = state.wallet(&ctx.env)?;
  let id = ctx.param("id").cloned().unwrap_or_default();
  let body: RecoverBody = req.json().await.unwrap_or_default();
  let Some(mut run) = load_run(&ctx.env, &id).await? else {
    return error(404, "no run with this id");
  };
  let rate = match body.fee_rate.map(FeeRate::new).transpose() {
    Ok(rate) => rate,
    Err(err) => return error(400, err),
  };
  let Some(recovery) = service::recovery(&state.client, &wallet, &run, rate, &mut OsRng)
    .await
    .map_err(to_worker)?
  else {
    return Response::from_json(&json!({ "id": id, "recovered": [] }));
  };
  let txid = service::broadcast_recovery(&state.client, &mut run, &recovery)
    .await
    .map_err(to_worker)?;
  store_run(&ctx.env, &run).await?;
  Response::from_json(&json!({
    "id": id,
    "recovery_txid": txid,
    "recovered": recovery.outputs.iter().map(|outpoint| outpoint.to_string()).collect::<Vec<_>>(),
    "fee_sat": recovery.fee.to_sat(),
  }))
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
  if !authorized(&req, &env)? {
    return error(401, "a valid bearer token is required");
  }
  Router::new()
    .get_async("/v1/wallet", wallet_info)
    .get_async("/v1/fees", fees)
    .post_async("/v1/estimate", estimate)
    .post_async("/v1/runs", create_run)
    .get_async("/v1/runs/:id", get_run)
    .post_async("/v1/runs/:id/broadcast", broadcast_run)
    .post_async("/v1/runs/:id/recover", recover_run)
    .run(req, env)
    .await
}
