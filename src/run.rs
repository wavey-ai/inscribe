#[cfg(feature = "native")]
use std::fs;
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::{Hash, sha256};
use bitcoin::secp256k1::SecretKey;
use bitcoin::taproot::TapNodeHash;
use bitcoin::{Amount, Network, OutPoint, ScriptBuf, Transaction, TxOut};
use serde::{Deserialize, Serialize};

use crate::plan::{FeeRate, Plan};

/// The record of one commit and its reveals. The file holds the ephemeral
/// secret keys of the commit outputs, which `recover` needs if a reveal does
/// not confirm. The program writes the file before it broadcasts anything.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
  pub format: u32,
  pub network: Network,
  pub created_unix: u64,
  pub destination: String,
  pub change_address: String,
  pub postage_sat: u64,
  pub commit_fee_rate: f64,
  pub reveal_fee_rate: f64,
  pub commit: TxRecord,
  pub reveals: Vec<RevealRecord>,
  #[serde(default)]
  pub recovery: Option<TxRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxRecord {
  pub txid: String,
  pub hex: String,
  pub fee_sat: u64,
  pub vsize: u64,
  pub broadcast: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevealRecord {
  pub file: String,
  pub sha256: String,
  pub content_type: String,
  pub content_encoding: Option<String>,
  pub body_bytes: usize,
  pub inscription_id: String,
  pub commit_vout: u32,
  pub commit_value_sat: u64,
  pub commit_script_pubkey: String,
  pub secret_key: String,
  pub merkle_root: String,
  pub tx: TxRecord,
}

impl TxRecord {
  pub fn new(tx: &Transaction, fee: Amount, broadcast: bool) -> Self {
    Self {
      txid: tx.compute_txid().to_string(),
      hex: serialize_hex(tx),
      fee_sat: fee.to_sat(),
      vsize: tx.vsize() as u64,
      broadcast,
    }
  }

  pub fn transaction(&self) -> Result<Transaction> {
    deserialize_hex(&self.hex).with_context(|| format!("decode transaction {}", self.txid))
  }
}

impl RevealRecord {
  pub fn commit_outpoint(&self, commit_txid: &str) -> Result<OutPoint> {
    Ok(OutPoint::new(commit_txid.parse()?, self.commit_vout))
  }

  pub fn commit_output(&self) -> Result<TxOut> {
    Ok(TxOut {
      value: Amount::from_sat(self.commit_value_sat),
      script_pubkey: ScriptBuf::from_hex(&self.commit_script_pubkey)?,
    })
  }

  pub fn secret_key(&self) -> Result<SecretKey> {
    Ok(self.secret_key.parse()?)
  }

  pub fn merkle_root(&self) -> Result<TapNodeHash> {
    Ok(self.merkle_root.parse()?)
  }
}

pub struct RunInputs<'a> {
  pub network: Network,
  pub destination: String,
  pub change_address: String,
  pub postage: Amount,
  pub commit_fee_rate: FeeRate,
  pub reveal_fee_rate: FeeRate,
  pub files: &'a [String],
  pub raw_sha256: &'a [String],
}

impl Run {
  /// `created_unix` is the time in seconds since the Unix epoch. The caller
  /// supplies it, because WebAssembly hosts have no system clock in std.
  pub fn from_plan(plan: &Plan, inputs: RunInputs, created_unix: u64) -> Self {
    let reveals = plan
      .reveals
      .iter()
      .zip(inputs.files.iter().zip(inputs.raw_sha256))
      .map(|(reveal, (file, sha))| RevealRecord {
        file: file.clone(),
        sha256: sha.clone(),
        content_type: String::from_utf8_lossy(&reveal.inscription.content_type).into_owned(),
        content_encoding: reveal
          .inscription
          .content_encoding
          .as_ref()
          .map(|value| String::from_utf8_lossy(value).into_owned()),
        body_bytes: reveal.inscription.body.len(),
        inscription_id: reveal.inscription_id(),
        commit_vout: reveal.commit_vout,
        commit_value_sat: reveal.commit_output.value.to_sat(),
        commit_script_pubkey: reveal.commit_output.script_pubkey.to_hex_string(),
        secret_key: reveal.secret_key.display_secret().to_string(),
        merkle_root: reveal.merkle_root.to_string(),
        tx: TxRecord::new(&reveal.tx, reveal.fee, false),
      })
      .collect();

    Self {
      format: 1,
      network: inputs.network,
      created_unix,
      destination: inputs.destination,
      change_address: inputs.change_address,
      postage_sat: inputs.postage.to_sat(),
      commit_fee_rate: inputs.commit_fee_rate.sat_per_vb(),
      reveal_fee_rate: inputs.reveal_fee_rate.sat_per_vb(),
      commit: TxRecord::new(&plan.commit, plan.commit_fee, false),
      reveals,
      recovery: None,
    }
  }

  /// A copy with the commit-output secret keys removed, for API responses
  /// and logs.
  pub fn without_secrets(&self) -> Self {
    let mut run = self.clone();
    for reveal in &mut run.reveals {
      reveal.secret_key.clear();
    }
    run
  }

  /// A stable identifier: the creation time and the start of the commit txid.
  pub fn id(&self) -> String {
    format!("{}-{}", self.created_unix, &self.commit.txid[..16])
  }

  #[cfg(feature = "native")]
  pub fn default_path(&self, runs_dir: &Path) -> PathBuf {
    runs_dir.join(format!(
      "{}-{}.json",
      self.created_unix,
      &self.commit.txid[..16]
    ))
  }

  #[cfg(feature = "native")]
  pub fn load(path: &Path) -> Result<Self> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("decode {}", path.display()))
  }

  /// Writes the file with owner-only permissions and replaces it atomically.
  #[cfg(feature = "native")]
  pub fn save(&self, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let _ = fs::remove_file(&tmp);
    crate::wallet::store::write_private(&tmp, serde_json::to_string_pretty(self)?.as_bytes())?;
    fs::rename(&tmp, path)
      .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
  }
}

pub fn sha256_hex(data: &[u8]) -> String {
  sha256::Hash::hash(data).to_string()
}
