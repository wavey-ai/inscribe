//! The inscription workflow on top of the planner and the chain client:
//! funding selection, run creation, broadcast, status, and recovery. Every
//! function is async and generic over the HTTP backend, and takes the clock
//! and the random number generator from the caller.

use anyhow::{Result, bail, ensure};
use bitcoin::secp256k1::rand::{CryptoRng, Rng};
use bitcoin::{Address, Amount, OutPoint, Transaction};
use serde::Serialize;

use crate::api::{Client, Http};
use crate::envelope::Inscription;
use crate::plan::{self, FeeRate, FundingUtxo, Params, Plan};
use crate::run::{self, Run, RunInputs, TxRecord};
use crate::wallet::Wallet;

/// One file to inscribe.
#[derive(Clone, Debug)]
pub struct FileInput {
  /// The file name. The extension sets the content type when `content_type` is None.
  pub name: String,
  pub bytes: Vec<u8>,
  pub content_type: Option<String>,
}

/// Files that are ready for a plan, in order.
#[derive(Clone, Debug)]
pub struct Prepared {
  pub inscriptions: Vec<Inscription>,
  pub names: Vec<String>,
  /// The SHA-256 of each file before compression.
  pub sha256: Vec<String>,
}

pub fn prepare(
  files: Vec<FileInput>,
  compress: bool,
  metaprotocol: Option<&str>,
) -> Result<Prepared> {
  ensure!(!files.is_empty(), "no files to inscribe");
  let mut prepared = Prepared {
    inscriptions: Vec::new(),
    names: Vec::new(),
    sha256: Vec::new(),
  };
  for file in files {
    prepared.sha256.push(run::sha256_hex(&file.bytes));
    prepared.inscriptions.push(Inscription::from_named_bytes(
      &file.name,
      file.bytes,
      file.content_type.as_deref(),
      compress,
      metaprotocol,
    )?);
    prepared.names.push(file.name);
  }
  Ok(prepared)
}

/// The explicit rate, or the mempool.space "half hour" rate.
pub async fn fee_rate<H: Http>(client: &Client<H>, explicit: Option<FeeRate>) -> Result<FeeRate> {
  match explicit {
    Some(rate) => Ok(rate),
    None => FeeRate::new(client.recommended_fees().await?.half_hour_fee),
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
  /// The output has no confirmation.
  Unconfirmed,
  /// The ord server reports an inscription or runes on the output.
  HoldsInscriptionOrRunes,
  /// The ord server has not indexed the output.
  NotIndexed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Skipped {
  pub outpoint: OutPoint,
  pub reason: SkipReason,
}

/// The funding outputs that are safe to spend. With `ord_check`, the ord
/// server must report each output as plain bitcoin.
pub async fn funding<H: Http>(
  client: &Client<H>,
  wallet: &Wallet,
  include_unconfirmed: bool,
  ord_check: bool,
) -> Result<(Vec<FundingUtxo>, Vec<Skipped>)> {
  if ord_check && !client.has_ord() {
    bail!("the ord check needs an ord server; set one, or turn the check off on a test network");
  }
  let mut usable = Vec::new();
  let mut skipped = Vec::new();
  for utxo in client.utxos(&wallet.address()).await? {
    let outpoint = utxo.outpoint();
    if !utxo.status.confirmed && !include_unconfirmed {
      skipped.push(Skipped {
        outpoint,
        reason: SkipReason::Unconfirmed,
      });
      continue;
    }
    if ord_check {
      let reason = match client.ord_utxo(&outpoint).await? {
        Some(info) if info.is_plain() => None,
        Some(_) => Some(SkipReason::HoldsInscriptionOrRunes),
        None => Some(SkipReason::NotIndexed),
      };
      if let Some(reason) = reason {
        skipped.push(Skipped { outpoint, reason });
        continue;
      }
    }
    usable.push(FundingUtxo {
      outpoint,
      value: utxo.amount(),
    });
  }
  Ok((usable, skipped))
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
  pub destination: Address,
  pub postage: Amount,
  /// None selects the mempool.space "half hour" rate.
  pub reveal_fee_rate: Option<FeeRate>,
  /// None selects the reveal fee rate.
  pub commit_fee_rate: Option<FeeRate>,
  pub include_unconfirmed: bool,
  pub ord_check: bool,
}

pub struct Created {
  pub run: Run,
  pub plan: Plan,
  pub skipped: Vec<Skipped>,
}

/// Builds and signs a run. Nothing goes to the network. Store the run before
/// a broadcast, because it holds the keys of the commit outputs.
pub async fn create<H: Http, R: Rng + CryptoRng>(
  client: &Client<H>,
  wallet: &Wallet,
  prepared: Prepared,
  options: &CreateOptions,
  created_unix: u64,
  rng: &mut R,
) -> Result<Created> {
  ensure!(
    options
      .destination
      .as_unchecked()
      .is_valid_for_network(wallet.network()),
    "the destination address is for a different network"
  );
  let reveal_fee_rate = fee_rate(client, options.reveal_fee_rate).await?;
  let commit_fee_rate = options.commit_fee_rate.unwrap_or(reveal_fee_rate);
  let (utxos, skipped) = funding(
    client,
    wallet,
    options.include_unconfirmed,
    options.ord_check,
  )
  .await?;

  let params = Params {
    destination: options.destination.clone(),
    change: wallet.address(),
    postage: options.postage,
    commit_fee_rate,
    reveal_fee_rate,
  };
  let plan = plan::build(
    wallet.secp(),
    wallet.keypair(),
    &utxos,
    prepared.inscriptions,
    &params,
    rng,
  )?;
  let run = Run::from_plan(
    &plan,
    RunInputs {
      network: wallet.network(),
      destination: options.destination.to_string(),
      change_address: wallet.address().to_string(),
      postage: options.postage,
      commit_fee_rate,
      reveal_fee_rate,
      files: &prepared.names,
      raw_sha256: &prepared.sha256,
    },
    created_unix,
  );
  Ok(Created { run, plan, skipped })
}

#[derive(Clone, Debug, Serialize)]
pub struct RevealFailure {
  pub file: String,
  pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BroadcastReport {
  pub commit_txid: String,
  pub revealed: Vec<String>,
  pub failures: Vec<RevealFailure>,
}

/// Sends the commit and then each reveal that the network does not have. The
/// function sets the `broadcast` flags in `run`; store the run afterwards.
/// A repeat call is safe: the function asks the network about each
/// transaction first. It does not trust the stored flags, because a mempool
/// can drop a transaction after it accepts it.
pub async fn broadcast<H: Http>(client: &Client<H>, run: &mut Run) -> Result<BroadcastReport> {
  let commit_txid = run.commit.txid.parse()?;
  if client.tx_status(&commit_txid).await?.is_none() {
    client.broadcast(&run.commit.transaction()?).await?;
  }
  run.commit.broadcast = true;

  let mut report = BroadcastReport {
    commit_txid: run.commit.txid.clone(),
    revealed: Vec::new(),
    failures: Vec::new(),
  };
  for record in run.reveals.iter_mut() {
    let txid = record.tx.txid.parse()?;
    let result = match client.tx_status(&txid).await {
      Ok(Some(_)) => Ok(()),
      Ok(None) => match record.tx.transaction() {
        Ok(tx) => client.broadcast(&tx).await.map(|_| ()),
        Err(err) => Err(err),
      },
      Err(err) => Err(err),
    };
    match result {
      Ok(()) => {
        record.tx.broadcast = true;
        report.revealed.push(record.inscription_id.clone());
      }
      Err(err) => report.failures.push(RevealFailure {
        file: record.file.clone(),
        error: format!("{err:#}"),
      }),
    }
  }
  Ok(report)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxState {
  NotInNetwork,
  Mempool,
  Confirmed { height: u32 },
}

#[derive(Clone, Debug, Serialize)]
pub struct RevealState {
  pub file: String,
  pub inscription_id: String,
  pub state: TxState,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunStatus {
  pub commit_txid: String,
  pub commit: TxState,
  pub reveals: Vec<RevealState>,
  pub recovery: Option<TxState>,
}

async fn tx_state<H: Http>(client: &Client<H>, txid: &str) -> Result<TxState> {
  Ok(match client.tx_status(&txid.parse()?).await? {
    None => TxState::NotInNetwork,
    Some(status) if status.confirmed => TxState::Confirmed {
      height: status.block_height.unwrap_or_default(),
    },
    Some(_) => TxState::Mempool,
  })
}

pub async fn status<H: Http>(client: &Client<H>, run: &Run) -> Result<RunStatus> {
  let mut reveals = Vec::new();
  for record in &run.reveals {
    reveals.push(RevealState {
      file: record.file.clone(),
      inscription_id: record.inscription_id.clone(),
      state: tx_state(client, &record.tx.txid).await?,
    });
  }
  let recovery = match &run.recovery {
    Some(record) => Some(tx_state(client, &record.txid).await?),
    None => None,
  };
  Ok(RunStatus {
    commit_txid: run.commit.txid.clone(),
    commit: tx_state(client, &run.commit.txid).await?,
    reveals,
    recovery,
  })
}

pub struct Recovery {
  pub tx: Transaction,
  pub fee: Amount,
  pub outputs: Vec<OutPoint>,
}

/// Builds a transaction that returns the unrevealed, unspent commit outputs
/// of `run` to the wallet. Returns None when there is nothing to recover.
pub async fn recovery<H: Http, R: Rng + CryptoRng>(
  client: &Client<H>,
  wallet: &Wallet,
  run: &Run,
  rate: Option<FeeRate>,
  rng: &mut R,
) -> Result<Option<Recovery>> {
  ensure!(
    run.network == wallet.network(),
    "the run is for {}, not {}",
    run.network,
    wallet.network()
  );
  let mut commits = Vec::new();
  for record in &run.reveals {
    let outpoint = record.commit_outpoint(&run.commit.txid)?;
    if client.tx_status(&record.tx.txid.parse()?).await?.is_some() {
      continue;
    }
    if client.outspend(&outpoint).await?.spent {
      continue;
    }
    commits.push((
      outpoint,
      record.commit_output()?,
      record.secret_key()?,
      record.merkle_root()?,
    ));
  }
  if commits.is_empty() {
    return Ok(None);
  }
  let rate = fee_rate(client, rate).await?;
  let (tx, fee) = plan::build_recovery(wallet.secp(), &commits, &wallet.address(), rate, rng)?;
  Ok(Some(Recovery {
    tx,
    fee,
    outputs: commits
      .iter()
      .map(|(outpoint, _, _, _)| *outpoint)
      .collect(),
  }))
}

/// Broadcasts a recovery transaction and records it in the run.
pub async fn broadcast_recovery<H: Http>(
  client: &Client<H>,
  run: &mut Run,
  recovery: &Recovery,
) -> Result<String> {
  let txid = client.broadcast(&recovery.tx).await?;
  run.recovery = Some(TxRecord::new(&recovery.tx, recovery.fee, true));
  Ok(txid.to_string())
}
