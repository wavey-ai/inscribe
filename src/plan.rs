use anyhow::{Result, bail, ensure};
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, TapTweak, UntweakedKeypair};
use bitcoin::policy::MAX_STANDARD_TX_WEIGHT;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey, Signing, Verification};
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::{
  ControlBlock, LeafVersion, Signature, TapLeafHash, TapNodeHash, TaprootBuilder,
};
use bitcoin::transaction::Version;
use bitcoin::{
  Address, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Weight, Witness,
};

use crate::envelope::Inscription;

const SCHNORR_SIGNATURE_SIZE: usize = 64;

/// The default mempool policy accepts at most 25 transactions in one chain of
/// unconfirmed descendants. One commit and 24 reveals fill that limit.
pub const MAX_REVEALS_PER_COMMIT: usize = 24;

/// A fee rate in satoshis per virtual byte. Fractional rates are valid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeeRate(f64);

impl FeeRate {
  pub fn new(sat_per_vb: f64) -> Result<Self> {
    ensure!(
      sat_per_vb.is_finite() && sat_per_vb > 0.0,
      "the fee rate must be more than 0 sat/vB"
    );
    Ok(Self(sat_per_vb))
  }

  pub fn sat_per_vb(self) -> f64 {
    self.0
  }

  pub fn fee(self, vsize: u64) -> Amount {
    Amount::from_sat((vsize as f64 * self.0).ceil() as u64)
  }
}

#[derive(Clone, Debug)]
pub struct FundingUtxo {
  pub outpoint: OutPoint,
  pub value: Amount,
}

#[derive(Clone, Debug)]
pub struct Params {
  pub destination: Address,
  pub change: Address,
  pub postage: Amount,
  pub commit_fee_rate: FeeRate,
  pub reveal_fee_rate: FeeRate,
}

/// The data that rebuilds the spend paths of one commit output.
#[derive(Clone, Debug)]
pub struct Reveal {
  pub inscription: Inscription,
  pub secret_key: SecretKey,
  pub merkle_root: TapNodeHash,
  pub commit_vout: u32,
  pub commit_output: TxOut,
  pub tx: Transaction,
  pub fee: Amount,
}

impl Reveal {
  pub fn inscription_id(&self) -> String {
    format!("{}i0", self.tx.compute_txid())
  }
}

#[derive(Clone, Debug)]
pub struct Plan {
  pub commit: Transaction,
  pub commit_prevouts: Vec<TxOut>,
  pub commit_fee: Amount,
  pub change: Option<Amount>,
  pub reveals: Vec<Reveal>,
}

impl Plan {
  pub fn total_spent(&self) -> Amount {
    self.commit_fee + self.reveals.iter().map(|reveal| reveal.fee).sum::<Amount>()
  }

  pub fn total_postage(&self) -> Amount {
    self
      .reveals
      .iter()
      .map(|reveal| reveal.tx.output[0].value)
      .sum::<Amount>()
  }
}

/// The parts of a reveal that do not depend on the commit transaction.
struct RevealTemplate {
  inscription: Inscription,
  keypair: UntweakedKeypair,
  script: ScriptBuf,
  control_block: ControlBlock,
  merkle_root: TapNodeHash,
  commit_script_pubkey: ScriptBuf,
  fee: Amount,
}

pub fn reveal_vsize(script: &ScriptBuf, control_block: &ControlBlock, output: &TxOut) -> u64 {
  let mut tx = reveal_skeleton(OutPoint::null(), output.clone());
  tx.input[0].witness.push([0u8; SCHNORR_SIGNATURE_SIZE]);
  tx.input[0].witness.push(script.as_bytes());
  tx.input[0].witness.push(control_block.serialize());
  tx.vsize() as u64
}

fn reveal_skeleton(previous_output: OutPoint, output: TxOut) -> Transaction {
  Transaction {
    version: Version::TWO,
    lock_time: LockTime::ZERO,
    input: vec![TxIn {
      previous_output,
      script_sig: ScriptBuf::new(),
      sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
      witness: Witness::new(),
    }],
    output: vec![output],
  }
}

fn template<C: Signing + Verification>(
  secp: &Secp256k1<C>,
  inscription: Inscription,
  params: &Params,
  keypair: UntweakedKeypair,
) -> Result<RevealTemplate> {
  let (key, _parity) = keypair.x_only_public_key();
  let script = inscription.reveal_script(&key);

  let spend_info = TaprootBuilder::new()
    .add_leaf(0, script.clone())
    .expect("one leaf at depth 0")
    .finalize(secp, key)
    .expect("a single-leaf tree is complete");
  let control_block = spend_info
    .control_block(&(script.clone(), LeafVersion::TapScript))
    .expect("the leaf is in the tree");
  let merkle_root = spend_info.merkle_root().expect("the tree has a leaf");
  let commit_script_pubkey = ScriptBuf::new_p2tr_tweaked(spend_info.output_key());

  let postage_output = TxOut {
    value: params.postage,
    script_pubkey: params.destination.script_pubkey(),
  };
  ensure!(
    params.postage >= postage_output.script_pubkey.minimal_non_dust(),
    "the postage of {} is below the dust limit of the destination address ({})",
    params.postage,
    postage_output.script_pubkey.minimal_non_dust()
  );

  let vsize = reveal_vsize(&script, &control_block, &postage_output);
  let weight = Weight::from_vb(vsize).expect("vsize fits in a weight");
  ensure!(
    weight <= Weight::from_wu(MAX_STANDARD_TX_WEIGHT.into()),
    "the reveal transaction for a {}-byte body has a weight of {} WU; the standard limit is {MAX_STANDARD_TX_WEIGHT} WU",
    inscription.body.len(),
    weight.to_wu()
  );

  Ok(RevealTemplate {
    fee: params.reveal_fee_rate.fee(vsize),
    inscription,
    keypair,
    script,
    control_block,
    merkle_root,
    commit_script_pubkey,
  })
}

fn commit_vsize(input_count: usize, outputs: &[TxOut]) -> u64 {
  let tx = Transaction {
    version: Version::TWO,
    lock_time: LockTime::ZERO,
    input: (0..input_count)
      .map(|_| TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::from_slice(&[[0u8; SCHNORR_SIGNATURE_SIZE]]),
      })
      .collect(),
    output: outputs.to_vec(),
  };
  tx.vsize() as u64
}

struct Selection {
  inputs: Vec<FundingUtxo>,
  fee: Amount,
  change: Option<Amount>,
}

/// Largest-first selection. A change output is added only when it is above
/// the dust limit; a smaller remainder goes to the miner.
fn select(
  utxos: &[FundingUtxo],
  outputs: &[TxOut],
  change_script: &ScriptBuf,
  rate: FeeRate,
) -> Result<Selection> {
  let mut sorted = utxos.to_vec();
  sorted.sort_by(|a, b| b.value.cmp(&a.value).then(a.outpoint.cmp(&b.outpoint)));

  let target: Amount = outputs.iter().map(|output| output.value).sum();
  let change_output = TxOut {
    value: Amount::ZERO,
    script_pubkey: change_script.clone(),
  };
  let dust = change_script.minimal_non_dust();

  let mut selected = Vec::new();
  let mut total = Amount::ZERO;
  for utxo in sorted {
    total += utxo.value;
    selected.push(utxo);

    let mut with_change = outputs.to_vec();
    with_change.push(change_output.clone());
    let fee_with_change = rate.fee(commit_vsize(selected.len(), &with_change));
    if let Some(change) = total.checked_sub(target + fee_with_change) {
      if change >= dust {
        return Ok(Selection {
          inputs: selected,
          fee: fee_with_change,
          change: Some(change),
        });
      }
    }

    let fee_without_change = rate.fee(commit_vsize(selected.len(), outputs));
    if total >= target + fee_without_change {
      return Ok(Selection {
        inputs: selected,
        fee: total - target,
        change: None,
      });
    }
  }

  let needed = target + rate.fee(commit_vsize(selected.len().max(1), outputs));
  bail!("the wallet has {total} in usable outputs; this plan needs at least {needed}",)
}

pub fn build<C: Signing + Verification>(
  secp: &Secp256k1<C>,
  funding_keypair: &Keypair,
  funding_utxos: &[FundingUtxo],
  inscriptions: Vec<Inscription>,
  params: &Params,
) -> Result<Plan> {
  ensure!(!inscriptions.is_empty(), "no files to inscribe");
  ensure!(
    inscriptions.len() <= MAX_REVEALS_PER_COMMIT,
    "{} files exceed the limit of {MAX_REVEALS_PER_COMMIT} reveals per commit; split the files into more runs",
    inscriptions.len()
  );

  let mut rng = bitcoin::secp256k1::rand::thread_rng();
  let templates = inscriptions
    .into_iter()
    .map(|inscription| template(secp, inscription, params, Keypair::new(secp, &mut rng)))
    .collect::<Result<Vec<_>>>()?;

  let commit_outputs: Vec<TxOut> = templates
    .iter()
    .map(|template| TxOut {
      value: template.fee + params.postage,
      script_pubkey: template.commit_script_pubkey.clone(),
    })
    .collect();

  let funding_script = ScriptBuf::new_p2tr(secp, funding_keypair.x_only_public_key().0, None);
  let change_script = params.change.script_pubkey();
  let selection = select(
    funding_utxos,
    &commit_outputs,
    &change_script,
    params.commit_fee_rate,
  )?;

  let mut outputs = commit_outputs.clone();
  if let Some(change) = selection.change {
    outputs.push(TxOut {
      value: change,
      script_pubkey: change_script,
    });
  }

  let mut commit = Transaction {
    version: Version::TWO,
    lock_time: LockTime::ZERO,
    input: selection
      .inputs
      .iter()
      .map(|utxo| TxIn {
        previous_output: utxo.outpoint,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::new(),
      })
      .collect(),
    output: outputs,
  };

  let commit_prevouts: Vec<TxOut> = selection
    .inputs
    .iter()
    .map(|utxo| TxOut {
      value: utxo.value,
      script_pubkey: funding_script.clone(),
    })
    .collect();

  sign_key_path(
    secp,
    &mut commit,
    &commit_prevouts,
    &funding_keypair.tap_tweak(secp, None).to_keypair(),
  )?;
  let commit_txid = commit.compute_txid();

  let reveals = templates
    .into_iter()
    .enumerate()
    .map(|(vout, template)| {
      let commit_output = commit.output[vout].clone();
      let tx = sign_reveal(
        secp,
        &template,
        OutPoint::new(commit_txid, vout as u32),
        &commit_output,
        params,
      )?;
      Ok(Reveal {
        inscription: template.inscription,
        secret_key: template.keypair.secret_key(),
        merkle_root: template.merkle_root,
        commit_vout: vout as u32,
        commit_output,
        fee: template.fee,
        tx,
      })
    })
    .collect::<Result<Vec<_>>>()?;

  Ok(Plan {
    commit,
    commit_prevouts,
    commit_fee: selection.fee,
    change: selection.change,
    reveals,
  })
}

fn sign_reveal<C: Signing + Verification>(
  secp: &Secp256k1<C>,
  template: &RevealTemplate,
  commit_outpoint: OutPoint,
  commit_output: &TxOut,
  params: &Params,
) -> Result<Transaction> {
  let mut tx = reveal_skeleton(
    commit_outpoint,
    TxOut {
      value: params.postage,
      script_pubkey: params.destination.script_pubkey(),
    },
  );

  let sighash = SighashCache::new(&tx).taproot_script_spend_signature_hash(
    0,
    &Prevouts::All(&[commit_output]),
    TapLeafHash::from_script(&template.script, LeafVersion::TapScript),
    TapSighashType::Default,
  )?;
  let signature = secp.sign_schnorr(
    &Message::from_digest(sighash.to_byte_array()),
    &template.keypair,
  );

  let witness = &mut tx.input[0].witness;
  witness.push(
    Signature {
      signature,
      sighash_type: TapSighashType::Default,
    }
    .to_vec(),
  );
  witness.push(template.script.as_bytes());
  witness.push(template.control_block.serialize());

  Ok(tx)
}

/// Signs every input as a BIP341 key-path spend with one tweaked key.
pub fn sign_key_path<C: Signing>(
  secp: &Secp256k1<C>,
  tx: &mut Transaction,
  prevouts: &[TxOut],
  tweaked: &Keypair,
) -> Result<()> {
  let mut cache = SighashCache::new(&mut *tx);
  let mut witnesses = Vec::new();
  for index in 0..prevouts.len() {
    let sighash = cache.taproot_key_spend_signature_hash(
      index,
      &Prevouts::All(prevouts),
      TapSighashType::Default,
    )?;
    let signature = secp.sign_schnorr(&Message::from_digest(sighash.to_byte_array()), tweaked);
    witnesses.push(Witness::p2tr_key_spend(&Signature {
      signature,
      sighash_type: TapSighashType::Default,
    }));
  }
  for (input, witness) in tx.input.iter_mut().zip(witnesses) {
    input.witness = witness;
  }
  Ok(())
}

/// Spends unrevealed commit outputs back to the wallet with the key path of
/// each commit output. The key path uses the ephemeral key tweaked with the
/// script tree root.
pub fn build_recovery<C: Signing + Verification>(
  secp: &Secp256k1<C>,
  commits: &[(OutPoint, TxOut, SecretKey, TapNodeHash)],
  destination: &Address,
  rate: FeeRate,
) -> Result<(Transaction, Amount)> {
  ensure!(!commits.is_empty(), "no commit outputs to recover");

  let total: Amount = commits.iter().map(|(_, output, _, _)| output.value).sum();
  let mut tx = Transaction {
    version: Version::TWO,
    lock_time: LockTime::ZERO,
    input: commits
      .iter()
      .map(|(outpoint, _, _, _)| TxIn {
        previous_output: *outpoint,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::from_slice(&[[0u8; SCHNORR_SIGNATURE_SIZE]]),
      })
      .collect(),
    output: vec![TxOut {
      value: total,
      script_pubkey: destination.script_pubkey(),
    }],
  };
  let fee = rate.fee(tx.vsize() as u64);
  let value = total
    .checked_sub(fee)
    .filter(|value| *value >= destination.script_pubkey().minimal_non_dust())
    .ok_or_else(|| {
      anyhow::anyhow!("the commit outputs ({total}) do not cover the recovery fee ({fee})")
    })?;
  tx.output[0].value = value;

  let prevouts: Vec<TxOut> = commits
    .iter()
    .map(|(_, output, _, _)| output.clone())
    .collect();
  let mut cache = SighashCache::new(&mut tx);
  let mut witnesses = Vec::new();
  for (index, (_, _, secret_key, merkle_root)) in commits.iter().enumerate() {
    let keypair = Keypair::from_secret_key(secp, secret_key).tap_tweak(secp, Some(*merkle_root));
    let sighash = cache.taproot_key_spend_signature_hash(
      index,
      &Prevouts::All(&prevouts),
      TapSighashType::Default,
    )?;
    let signature = secp.sign_schnorr(
      &Message::from_digest(sighash.to_byte_array()),
      &keypair.to_keypair(),
    );
    witnesses.push(Witness::p2tr_key_spend(&Signature {
      signature,
      sighash_type: TapSighashType::Default,
    }));
  }
  for (input, witness) in tx.input.iter_mut().zip(witnesses) {
    input.witness = witness;
  }

  Ok((tx, fee))
}
