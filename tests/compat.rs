use std::str::FromStr;

use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::script::{Builder, Instruction};
use bitcoin::taproot::ControlBlock;
use bitcoin::{
  Address, Amount, Network, OutPoint, ScriptBuf, Transaction, TxOut, Txid, XOnlyPublicKey,
};

use inscribe::envelope::{Inscription, Mode};
use inscribe::plan::{self, FeeRate, FundingUtxo, Params};
use inscribe::wallet::Wallet;

const MNEMONIC: &str =
  "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!(
    "{}/tests/fixtures/{name}",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

fn fixture_tx(name: &str) -> Transaction {
  deserialize_hex(String::from_utf8(fixture(name)).unwrap().trim()).unwrap()
}

/// Splits a mainnet reveal witness into the leaf key, the tapscript, and the
/// control block.
fn reveal_parts(tx: &Transaction) -> (XOnlyPublicKey, ScriptBuf, ControlBlock) {
  let witness = &tx.input[0].witness;
  assert_eq!(witness.len(), 3);
  let script = ScriptBuf::from_bytes(witness.nth(1).unwrap().to_vec());
  let key = match script.instructions().next().unwrap().unwrap() {
    Instruction::PushBytes(bytes) => XOnlyPublicKey::from_slice(bytes.as_bytes()).unwrap(),
    Instruction::Op(op) => panic!("unexpected {op}"),
  };
  let control_block = ControlBlock::decode(witness.nth(2).unwrap()).unwrap();
  (key, script, control_block)
}

fn check_mainnet_reveal(tx_name: &str, content_name: &str, content_type: &str) {
  let tx = fixture_tx(tx_name);
  let (key, script, control_block) = reveal_parts(&tx);

  let inscription = Inscription::new(
    content_type,
    fixture(content_name),
    false,
    Mode::Generic,
    None,
  )
  .unwrap();
  assert_eq!(
    inscription.reveal_script(&key),
    script,
    "{tx_name}: tapscript differs"
  );

  // The mainnet reveals also carry a change output and, for the PNG, a second
  // input. The estimate covers the one-input, one-output shape that this tool
  // builds, so the comparison drops the extra parts.
  let mut trimmed = tx.clone();
  trimmed.input.truncate(1);
  trimmed.output.truncate(1);
  let estimate = plan::reveal_vsize(&script, &control_block, &tx.output[0]);
  assert_eq!(
    estimate,
    trimmed.vsize() as u64,
    "{tx_name}: vsize estimate differs"
  );
}

#[test]
fn html_envelope_is_byte_identical_to_mainnet_inscription() {
  // 16695e97fc2425f4ef6d23cb4aafb473c9ebd699a50adea94624a6cd15ba29e1i0, block 850303.
  check_mainnet_reveal(
    "reveal-html.hex",
    "reveal-html.content",
    "text/html;charset=utf-8",
  );
}

#[test]
fn png_envelope_is_byte_identical_to_mainnet_inscription() {
  // 4bdd3d72da1fe299ed89cd2c71b29b901807d1b3d41d447aec8a5383eacddf84i0, block 849187.
  check_mainnet_reveal("reveal-png.hex", "reveal-png.content", "image/png");
}

fn verify_input(tx: &Transaction, index: usize, prevouts: &[TxOut]) {
  let serialized = bitcoin::consensus::encode::serialize(tx);
  let utxos: Vec<bitcoinconsensus::Utxo> = prevouts
    .iter()
    .map(|output| bitcoinconsensus::Utxo {
      script_pubkey: output.script_pubkey.as_bytes().as_ptr(),
      script_pubkey_len: output.script_pubkey.len() as u32,
      value: output.value.to_sat() as i64,
    })
    .collect();
  bitcoinconsensus::verify(
    prevouts[index].script_pubkey.as_bytes(),
    prevouts[index].value.to_sat(),
    &serialized,
    Some(&utxos),
    index,
  )
  .unwrap_or_else(|err| {
    panic!(
      "input {index} of {} fails consensus: {err:?}",
      tx.compute_txid()
    )
  });
}

fn test_plan(bodies: Vec<Vec<u8>>, funding: Vec<u64>, rate: f64) -> (Wallet, plan::Plan, Params) {
  let wallet = Wallet::from_mnemonic(
    &bip39::Mnemonic::from_str(MNEMONIC).unwrap(),
    Network::Signet,
  )
  .unwrap();
  let secp = Secp256k1::new();
  let destination = Address::p2tr(
    &secp,
    Keypair::new(&secp, &mut bitcoin::secp256k1::rand::thread_rng())
      .x_only_public_key()
      .0,
    None,
    Network::Signet,
  );
  let params = Params {
    destination,
    change: wallet.address(),
    postage: Amount::from_sat(546),
    commit_fee_rate: FeeRate::new(rate).unwrap(),
    reveal_fee_rate: FeeRate::new(rate).unwrap(),
  };
  let utxos: Vec<FundingUtxo> = funding
    .iter()
    .enumerate()
    .map(|(i, value)| FundingUtxo {
      outpoint: OutPoint::new(
        Txid::from_str(&format!("{:064x}", i + 1)).unwrap(),
        i as u32,
      ),
      value: Amount::from_sat(*value),
    })
    .collect();
  let inscriptions = bodies
    .into_iter()
    .map(|body| {
      Inscription::new("application/octet-stream", body, false, Mode::Generic, None).unwrap()
    })
    .collect();
  let built = plan::build(
    wallet.secp(),
    wallet.keypair(),
    &utxos,
    inscriptions,
    &params,
  )
  .unwrap();
  (wallet, built, params)
}

#[test]
fn commit_and_reveals_pass_consensus_verification() {
  let (_wallet, built, params) = test_plan(
    vec![vec![1; 1000], vec![2; 30_000], vec![3; 120_000]],
    vec![20_000, 900_000, 5_000],
    2.5,
  );

  for index in 0..built.commit.input.len() {
    verify_input(&built.commit, index, &built.commit_prevouts);
  }

  let commit_txid = built.commit.compute_txid();
  for (vout, reveal) in built.reveals.iter().enumerate() {
    assert_eq!(
      reveal.tx.input[0].previous_output,
      OutPoint::new(commit_txid, vout as u32)
    );
    assert_eq!(reveal.tx.output[0].value, params.postage);
    assert_eq!(
      reveal.tx.output[0].script_pubkey,
      params.destination.script_pubkey()
    );
    verify_input(&reveal.tx, 0, &[built.commit.output[vout].clone()]);

    // The commit output pays the reveal fee exactly at the requested rate.
    let paid = built.commit.output[vout].value - reveal.tx.output[0].value;
    assert_eq!(paid, reveal.fee);
    assert_eq!(paid, params.reveal_fee_rate.fee(reveal.tx.vsize() as u64));
  }

  // Inputs equal outputs plus the commit fee.
  let inputs: Amount = built
    .commit_prevouts
    .iter()
    .map(|output| output.value)
    .sum();
  let outputs: Amount = built.commit.output.iter().map(|output| output.value).sum();
  assert_eq!(inputs - outputs, built.commit_fee);
  assert!(built.commit_fee >= params.commit_fee_rate.fee(built.commit.vsize() as u64));
  // Largest-first selection uses the 900,000 sat output alone.
  assert_eq!(built.commit.input.len(), 1);
  assert!(built.change.is_some());
}

#[test]
fn recovery_spends_commit_outputs_by_key_path() {
  let (wallet, built, _params) = test_plan(vec![vec![9; 5000], vec![8; 6000]], vec![100_000], 3.0);
  let commit_txid = built.commit.compute_txid();
  let commits: Vec<_> = built
    .reveals
    .iter()
    .map(|reveal| {
      (
        OutPoint::new(commit_txid, reveal.commit_vout),
        reveal.commit_output.clone(),
        reveal.secret_key,
        reveal.merkle_root,
      )
    })
    .collect();
  let (tx, fee) = plan::build_recovery(
    wallet.secp(),
    &commits,
    &wallet.address(),
    FeeRate::new(3.0).unwrap(),
  )
  .unwrap();
  let prevouts: Vec<TxOut> = commits
    .iter()
    .map(|(_, output, _, _)| output.clone())
    .collect();
  for index in 0..tx.input.len() {
    verify_input(&tx, index, &prevouts);
  }
  let total: Amount = prevouts.iter().map(|output| output.value).sum();
  assert_eq!(tx.output[0].value + fee, total);
}

#[test]
fn small_remainder_goes_to_the_fee_instead_of_dust_change() {
  // Find a funding value that leaves less than the dust limit after one reveal.
  let (_, probe, _) = test_plan(vec![vec![5; 100]], vec![1_000_000], 1.0);
  let needed = probe.commit.output[0].value + probe.commit_fee - FeeRate::new(1.0).unwrap().fee(43); // the probe has a change output of 43 vB
  let (_, built, _) = test_plan(vec![vec![5; 100]], vec![needed.to_sat() + 100], 1.0);
  assert!(built.change.is_none());
  assert_eq!(built.commit.output.len(), 1);
}

#[test]
fn insufficient_funds_and_oversize_bodies_are_errors() {
  let wallet = Wallet::from_mnemonic(
    &bip39::Mnemonic::from_str(MNEMONIC).unwrap(),
    Network::Signet,
  )
  .unwrap();
  let params = Params {
    destination: wallet.address(),
    change: wallet.address(),
    postage: Amount::from_sat(546),
    commit_fee_rate: FeeRate::new(1.0).unwrap(),
    reveal_fee_rate: FeeRate::new(1.0).unwrap(),
  };
  let utxo = vec![FundingUtxo {
    outpoint: OutPoint::new(Txid::from_str(&format!("{:064x}", 7)).unwrap(), 0),
    value: Amount::from_sat(1_000),
  }];
  let small = Inscription::new(
    "text/plain;charset=utf-8",
    vec![b'a'; 10_000],
    false,
    Mode::Text,
    None,
  )
  .unwrap();
  let err = plan::build(wallet.secp(), wallet.keypair(), &utxo, vec![small], &params).unwrap_err();
  assert!(err.to_string().contains("needs at least"), "{err}");

  let huge = Inscription::new(
    "application/octet-stream",
    vec![0; 400_000],
    false,
    Mode::Generic,
    None,
  )
  .unwrap();
  let err = plan::build(wallet.secp(), wallet.keypair(), &utxo, vec![huge], &params).unwrap_err();
  assert!(err.to_string().contains("standard limit"), "{err}");

  let low_postage = Params {
    postage: Amount::from_sat(100),
    ..params.clone()
  };
  let tiny = Inscription::new(
    "text/plain;charset=utf-8",
    vec![b'a'; 10],
    false,
    Mode::Text,
    None,
  )
  .unwrap();
  let err = plan::build(
    wallet.secp(),
    wallet.keypair(),
    &utxo,
    vec![tiny],
    &low_postage,
  )
  .unwrap_err();
  assert!(err.to_string().contains("dust"), "{err}");
}

#[test]
fn envelope_round_trips_through_a_reference_parser() {
  let inscription = Inscription::new(
    "text/html;charset=utf-8",
    b"<p>x</p>".repeat(200),
    true,
    Mode::Text,
    Some("wavey"),
  )
  .unwrap();
  let script = inscription.append_to(Builder::new()).into_script();

  // The parser follows the ord envelope rules: OP_FALSE OP_IF "ord", then tag
  // and value pairs, then an empty push and the body chunks, then OP_ENDIF.
  let mut pushes = Vec::new();
  let mut ops = Vec::new();
  for instruction in script.instructions() {
    match instruction.unwrap() {
      Instruction::PushBytes(bytes) => pushes.push(bytes.as_bytes().to_vec()),
      Instruction::Op(op) => ops.push(op),
    }
  }
  assert_eq!(
    ops,
    vec![
      bitcoin::opcodes::all::OP_IF,
      bitcoin::opcodes::all::OP_ENDIF
    ]
  );
  assert_eq!(pushes[0], Vec::<u8>::new());
  assert_eq!(pushes[1], b"ord");
  let mut fields = std::collections::BTreeMap::new();
  let mut i = 2;
  while !pushes[i].is_empty() {
    fields.insert(pushes[i].clone(), pushes[i + 1].clone());
    i += 2;
  }
  let body: Vec<u8> = pushes[i + 1..].concat();
  assert_eq!(fields[&vec![1u8]], b"text/html;charset=utf-8");
  assert_eq!(fields[&vec![9u8]], b"br");
  assert_eq!(fields[&vec![7u8]], b"wavey");
  assert_eq!(body, inscription.body);
}
