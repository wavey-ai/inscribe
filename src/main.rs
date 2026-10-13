use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Amount, Network, OutPoint};
use clap::{Args, Parser, Subcommand, ValueEnum};

use inscribe::api::{self, Esplora, Ord};
use inscribe::envelope::Inscription;
use inscribe::plan::{self, FeeRate, FundingUtxo, Params};
use inscribe::run::{self, Run, RunInputs};
use inscribe::wallet::Wallet;

#[derive(Parser)]
#[command(
  version,
  about = "Create Bitcoin ordinal inscriptions with a local key"
)]
struct Cli {
  /// The Bitcoin network.
  #[arg(
    long,
    value_enum,
    env = "INSCRIBE_NETWORK",
    default_value = "bitcoin",
    global = true
  )]
  network: Chain,

  /// The data directory. Each network has a subdirectory.
  #[arg(long, env = "INSCRIBE_HOME", global = true)]
  home: Option<PathBuf>,

  /// The Esplora REST endpoint. The default is mempool.space for the network.
  #[arg(long, env = "INSCRIBE_ESPLORA_URL", global = true)]
  esplora_url: Option<String>,

  /// The ord server that checks funding outputs for inscriptions and runes.
  /// The default on mainnet is https://ordinals.com.
  #[arg(long, env = "INSCRIBE_ORD_URL", global = true)]
  ord_url: Option<String>,

  #[command(subcommand)]
  command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Chain {
  Bitcoin,
  Signet,
  Testnet4,
  Regtest,
}

impl Chain {
  fn network(self) -> Network {
    match self {
      Chain::Bitcoin => Network::Bitcoin,
      Chain::Signet => Network::Signet,
      Chain::Testnet4 => Network::Testnet4,
      Chain::Regtest => Network::Regtest,
    }
  }

  fn dir(self) -> &'static str {
    match self {
      Chain::Bitcoin => "bitcoin",
      Chain::Signet => "signet",
      Chain::Testnet4 => "testnet4",
      Chain::Regtest => "regtest",
    }
  }
}

#[derive(Subcommand)]
enum Command {
  /// Manage the funding wallet.
  #[command(subcommand)]
  Wallet(WalletCommand),
  /// Show the size and cost of inscribing files. This needs no wallet.
  Estimate(EstimateArgs),
  /// Build, sign, save, and broadcast a commit and its reveals.
  Create(CreateArgs),
  /// Broadcast the transactions of a run that the network does not have yet.
  Broadcast { run: PathBuf },
  /// Show the state of a run.
  Status { run: PathBuf },
  /// Return unrevealed commit outputs of a run to the wallet.
  Recover(RecoverArgs),
}

#[derive(Subcommand)]
enum WalletCommand {
  /// Make a new 24-word mnemonic and save it in the data directory.
  Init,
  /// Show the funding address.
  Address,
  /// Show the funding outputs and the balance.
  Balance,
  /// Show the BIP86 output descriptor for watch-only wallets.
  Descriptor,
  /// Show the mnemonic.
  Mnemonic,
}

#[derive(Args)]
struct ContentArgs {
  /// The files to inscribe, in order.
  #[arg(required = true)]
  files: Vec<PathBuf>,

  /// The content type for all files. The default comes from each file extension.
  #[arg(long)]
  content_type: Option<String>,

  /// Compress each body with brotli when the result is smaller.
  #[arg(long)]
  compress: bool,

  /// The metaprotocol field (tag 7) for all files.
  #[arg(long)]
  metaprotocol: Option<String>,

  /// The value of each inscription output, in satoshis.
  #[arg(long, default_value_t = 546)]
  postage: u64,
}

#[derive(Args)]
struct FeeArgs {
  /// The fee rate in sat/vB. The default is the mempool.space "half hour" rate.
  #[arg(long)]
  fee_rate: Option<f64>,

  /// The commit fee rate in sat/vB. The default is the --fee-rate value.
  #[arg(long)]
  commit_fee_rate: Option<f64>,
}

#[derive(Args)]
struct EstimateArgs {
  #[command(flatten)]
  content: ContentArgs,
  #[command(flatten)]
  fees: FeeArgs,
}

#[derive(Args)]
struct CreateArgs {
  #[command(flatten)]
  content: ContentArgs,
  #[command(flatten)]
  fees: FeeArgs,

  /// The address that receives the inscriptions.
  #[arg(long)]
  destination: Address<NetworkUnchecked>,

  /// Spend funding outputs that have no confirmation.
  #[arg(long)]
  include_unconfirmed: bool,

  /// Spend funding outputs without the ord inscription and rune check.
  #[arg(long)]
  no_ord_check: bool,

  /// Save the signed transactions and stop before the broadcast.
  #[arg(long)]
  dry_run: bool,

  /// Broadcast with no confirmation prompt.
  #[arg(long)]
  yes: bool,
}

#[derive(Args)]
struct RecoverArgs {
  run: PathBuf,

  /// The fee rate in sat/vB. The default is the mempool.space "half hour" rate.
  #[arg(long)]
  fee_rate: Option<f64>,

  /// Broadcast with no confirmation prompt.
  #[arg(long)]
  yes: bool,
}

struct App {
  network: Network,
  home: PathBuf,
  esplora: Option<Esplora>,
  ord: Option<Ord>,
}

impl App {
  fn new(cli: &Cli) -> Result<Self> {
    let network = cli.network.network();
    let root = match &cli.home {
      Some(home) => home.clone(),
      None => std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".inscribe"))
        .ok_or_else(|| anyhow!("HOME is not set; use --home"))?,
    };
    let esplora = match cli
      .esplora_url
      .as_deref()
      .or(api::default_esplora_url(network))
    {
      Some(url) => Some(Esplora::new(url)?),
      None => None,
    };
    let ord = match cli.ord_url.as_deref().or(api::default_ord_url(network)) {
      Some(url) => Some(Ord::new(url)?),
      None => None,
    };
    Ok(Self {
      network,
      home: root.join(cli.network.dir()),
      esplora,
      ord,
    })
  }

  fn esplora(&self) -> Result<&Esplora> {
    self
      .esplora
      .as_ref()
      .ok_or_else(|| anyhow!("no Esplora endpoint for this network; use --esplora-url"))
  }

  fn runs_dir(&self) -> PathBuf {
    self.home.join("runs")
  }

  fn wallet(&self) -> Result<Wallet> {
    Wallet::load(&self.home, self.network)
  }

  fn fee_rate(&self, explicit: Option<f64>) -> Result<FeeRate> {
    match explicit {
      Some(rate) => FeeRate::new(rate),
      None => {
        let fees = self
          .esplora()?
          .recommended_fees()
          .context("get the recommended fee rate; or set --fee-rate")?;
        FeeRate::new(fees.half_hour_fee)
      }
    }
  }

  fn fiat(&self, amount: Amount) -> String {
    if self.network != Network::Bitcoin {
      return String::new();
    }
    match self.esplora.as_ref().map(|esplora| esplora.prices()) {
      Some(Ok(prices)) => {
        let btc = amount.to_btc();
        format!(" (£{:.2}, ${:.2})", btc * prices.gbp, btc * prices.usd)
      }
      _ => String::new(),
    }
  }
}

fn main() {
  if let Err(err) = run_cli() {
    eprintln!("error: {err:#}");
    std::process::exit(1);
  }
}

fn run_cli() -> Result<()> {
  let cli = Cli::parse();
  let ctx = App::new(&cli)?;

  match cli.command {
    Command::Wallet(command) => wallet_command(&ctx, command),
    Command::Estimate(args) => estimate(&ctx, args),
    Command::Create(args) => create(&ctx, args),
    Command::Broadcast { run } => broadcast(&ctx, &run),
    Command::Status { run } => status(&ctx, &run),
    Command::Recover(args) => recover(&ctx, args),
  }
}

fn wallet_command(ctx: &App, command: WalletCommand) -> Result<()> {
  match command {
    WalletCommand::Init => {
      let (wallet, _mnemonic) = Wallet::create(&ctx.home, ctx.network)?;
      println!(
        "Saved a new mnemonic in {}",
        inscribe::wallet::mnemonic_path(&ctx.home).display()
      );
      println!(
        "Back up the mnemonic now: inscribe --network {} wallet mnemonic",
        network_arg(ctx.network)
      );
      println!("Funding address: {}", wallet.address());
      println!("Descriptor: {}", wallet.descriptor());
    }
    WalletCommand::Address => println!("{}", ctx.wallet()?.address()),
    WalletCommand::Descriptor => println!("{}", ctx.wallet()?.descriptor()),
    WalletCommand::Mnemonic => println!("{}", Wallet::load_mnemonic(&ctx.home)?),
    WalletCommand::Balance => {
      let wallet = ctx.wallet()?;
      let utxos = ctx.esplora()?.utxos(&wallet.address())?;
      let mut confirmed = Amount::ZERO;
      let mut unconfirmed = Amount::ZERO;
      for utxo in &utxos {
        let state = if utxo.status.confirmed {
          confirmed += utxo.amount();
          "confirmed"
        } else {
          unconfirmed += utxo.amount();
          "unconfirmed"
        };
        println!("{} {:>14} {state}", utxo.outpoint(), utxo.amount().to_sat());
      }
      println!("Confirmed: {confirmed}{}", ctx.fiat(confirmed));
      println!("Unconfirmed: {unconfirmed}");
    }
  }
  Ok(())
}

fn network_arg(network: Network) -> &'static str {
  match network {
    Network::Bitcoin => "bitcoin",
    Network::Signet => "signet",
    Network::Testnet4 => "testnet4",
    _ => "regtest",
  }
}

struct LoadedFiles {
  inscriptions: Vec<Inscription>,
  names: Vec<String>,
  sha256: Vec<String>,
}

fn load_files(args: &ContentArgs) -> Result<LoadedFiles> {
  let mut loaded = LoadedFiles {
    inscriptions: Vec::new(),
    names: Vec::new(),
    sha256: Vec::new(),
  };
  for path in &args.files {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    loaded.sha256.push(run::sha256_hex(&raw));
    loaded.names.push(path.display().to_string());
    loaded.inscriptions.push(Inscription::from_file(
      path,
      args.content_type.as_deref(),
      args.compress,
      args.metaprotocol.as_deref(),
    )?);
  }
  Ok(loaded)
}

fn estimate(ctx: &App, args: EstimateArgs) -> Result<()> {
  let files = load_files(&args.content)?;
  let reveal_rate = ctx.fee_rate(args.fees.fee_rate)?;
  let commit_rate = match args.fees.commit_fee_rate {
    Some(rate) => FeeRate::new(rate)?,
    None => reveal_rate,
  };

  // A throwaway key and a P2TR placeholder give the exact reveal sizes. The
  // commit estimate assumes one funding input and one change output.
  let secp = bitcoin::secp256k1::Secp256k1::new();
  let placeholder = Address::p2tr(
    &secp,
    bitcoin::key::Keypair::new(&secp, &mut bitcoin::secp256k1::rand::thread_rng())
      .x_only_public_key()
      .0,
    None,
    ctx.network,
  );
  let wallet_key = bitcoin::key::Keypair::new(&secp, &mut bitcoin::secp256k1::rand::thread_rng());
  let params = Params {
    destination: placeholder.clone(),
    change: placeholder,
    postage: Amount::from_sat(args.content.postage),
    commit_fee_rate: commit_rate,
    reveal_fee_rate: reveal_rate,
  };
  let funding = [FundingUtxo {
    outpoint: OutPoint::new(
      bitcoin::Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros()),
      0,
    ),
    value: Amount::MAX_MONEY,
  }];
  let plan = plan::build(&secp, &wallet_key, &funding, files.inscriptions, &params)?;

  println!(
    "Reveal fee rate {} sat/vB, commit fee rate {} sat/vB",
    reveal_rate.sat_per_vb(),
    commit_rate.sat_per_vb()
  );
  for (reveal, name) in plan.reveals.iter().zip(&files.names) {
    println!(
      "{name}: {} bytes{}, reveal {} vB, fee {} sat",
      reveal.inscription.body.len(),
      if reveal.inscription.content_encoding.is_some() {
        " (brotli)"
      } else {
        ""
      },
      reveal.tx.vsize(),
      reveal.fee.to_sat()
    );
  }
  println!(
    "Commit: {} vB, fee {} sat",
    plan.commit.vsize(),
    plan.commit_fee.to_sat()
  );
  let fees = plan.total_spent();
  println!("Network fees: {} sat{}", fees.to_sat(), ctx.fiat(fees));
  println!(
    "Postage held in the inscriptions: {} sat",
    plan.total_postage().to_sat()
  );
  Ok(())
}

fn funding_utxos(ctx: &App, wallet: &Wallet, args: &CreateArgs) -> Result<Vec<FundingUtxo>> {
  let utxos = ctx.esplora()?.utxos(&wallet.address())?;
  let mut usable = Vec::new();
  for utxo in utxos {
    if !utxo.status.confirmed && !args.include_unconfirmed {
      eprintln!("skip {}: no confirmation", utxo.outpoint());
      continue;
    }
    if !args.no_ord_check {
      let ord = ctx.ord.as_ref().ok_or_else(|| {
        anyhow!(
          "no ord server for this network; use --ord-url, or --no-ord-check on a test network"
        )
      })?;
      match ord.utxo(&utxo.outpoint())? {
        Some(info) if info.is_plain() => {}
        Some(_) => {
          eprintln!("skip {}: it holds an inscription or runes", utxo.outpoint());
          continue;
        }
        None => {
          eprintln!(
            "skip {}: the ord server has not indexed it",
            utxo.outpoint()
          );
          continue;
        }
      }
    }
    usable.push(FundingUtxo {
      outpoint: utxo.outpoint(),
      value: utxo.amount(),
    });
  }
  Ok(usable)
}

fn confirm(prompt: &str) -> Result<bool> {
  print!("{prompt} Type yes to continue: ");
  io::stdout().flush()?;
  let mut line = String::new();
  io::stdin().lock().read_line(&mut line)?;
  Ok(line.trim() == "yes")
}

fn create(ctx: &App, args: CreateArgs) -> Result<()> {
  let wallet = ctx.wallet()?;
  let destination = args
    .destination
    .clone()
    .require_network(ctx.network)
    .context("the destination address is for a different network")?;
  let files = load_files(&args.content)?;
  let reveal_rate = ctx.fee_rate(args.fees.fee_rate)?;
  let commit_rate = match args.fees.commit_fee_rate {
    Some(rate) => FeeRate::new(rate)?,
    None => reveal_rate,
  };
  let postage = Amount::from_sat(args.content.postage);

  let funding = funding_utxos(ctx, &wallet, &args)?;
  let params = Params {
    destination: destination.clone(),
    change: wallet.address(),
    postage,
    commit_fee_rate: commit_rate,
    reveal_fee_rate: reveal_rate,
  };
  let plan = plan::build(
    wallet.secp(),
    wallet.keypair(),
    &funding,
    files.inscriptions,
    &params,
  )?;

  let run = Run::from_plan(
    &plan,
    RunInputs {
      network: ctx.network,
      destination: destination.to_string(),
      change_address: wallet.address().to_string(),
      postage,
      commit_fee_rate: commit_rate,
      reveal_fee_rate: reveal_rate,
      files: &files.names,
      raw_sha256: &files.sha256,
    },
  );
  let path = run.default_path(&ctx.runs_dir());
  run.save(&path)?;

  println!("Network: {}", network_arg(ctx.network));
  println!("Destination: {destination}");
  for record in &run.reveals {
    println!(
      "{} -> {} ({} bytes, {} vB, fee {} sat)",
      record.file, record.inscription_id, record.body_bytes, record.tx.vsize, record.tx.fee_sat
    );
  }
  println!(
    "Commit {}: {} vB, fee {} sat",
    run.commit.txid, run.commit.vsize, run.commit.fee_sat
  );
  if let Some(change) = plan.change {
    println!("Change to the wallet: {} sat", change.to_sat());
  }
  let fees = plan.total_spent();
  println!("Network fees: {} sat{}", fees.to_sat(), ctx.fiat(fees));
  println!(
    "Postage held in the inscriptions: {} sat",
    plan.total_postage().to_sat()
  );
  println!("Run file: {}", path.display());

  if args.dry_run {
    println!(
      "Dry run: nothing was broadcast. To broadcast later: inscribe --network {} broadcast {}",
      network_arg(ctx.network),
      path.display()
    );
    return Ok(());
  }
  if !args.yes && !confirm("Broadcast these transactions?")? {
    println!("Nothing was broadcast. The run file stays for a later broadcast.");
    return Ok(());
  }
  broadcast(ctx, &path)
}

fn broadcast(ctx: &App, path: &Path) -> Result<()> {
  let mut run = Run::load(path)?;
  if run.network != ctx.network {
    bail!("the run is for {}, not {}", run.network, ctx.network);
  }
  let esplora = ctx.esplora()?;

  let commit_txid = run.commit.txid.parse()?;
  if !run.commit.broadcast {
    if esplora.tx_status(&commit_txid)?.is_none() {
      esplora.broadcast(&run.commit.transaction()?)?;
    }
    run.commit.broadcast = true;
    run.save(path)?;
  }
  println!("Commit {} is in the network", run.commit.txid);

  let mut failures = 0;
  for index in 0..run.reveals.len() {
    if run.reveals[index].tx.broadcast {
      continue;
    }
    let txid = run.reveals[index].tx.txid.parse()?;
    let result = match esplora.tx_status(&txid)? {
      Some(_) => Ok(()),
      None => esplora
        .broadcast(&run.reveals[index].tx.transaction()?)
        .map(|_| ()),
    };
    match result {
      Ok(()) => {
        run.reveals[index].tx.broadcast = true;
        run.save(path)?;
        println!(
          "Reveal {} -> {}",
          run.reveals[index].file, run.reveals[index].inscription_id
        );
      }
      Err(err) => {
        failures += 1;
        eprintln!("Reveal {} failed: {err:#}", run.reveals[index].file);
      }
    }
  }
  if failures > 0 {
    bail!(
      "{failures} reveal(s) failed. Run `broadcast` again, or `recover` to return the unrevealed commit outputs to the wallet."
    );
  }
  Ok(())
}

fn status(ctx: &App, path: &Path) -> Result<()> {
  let run = Run::load(path)?;
  let esplora = ctx.esplora()?;
  let describe = |txid: &str| -> Result<String> {
    Ok(match esplora.tx_status(&txid.parse()?)? {
      None => "not in the network".to_string(),
      Some(status) if status.confirmed => format!(
        "confirmed at height {}",
        status.block_height.unwrap_or_default()
      ),
      Some(_) => "in the mempool".to_string(),
    })
  };
  println!(
    "Commit {}: {}",
    run.commit.txid,
    describe(&run.commit.txid)?
  );
  for record in &run.reveals {
    println!(
      "{} {}: {}",
      record.inscription_id,
      record.file,
      describe(&record.tx.txid)?
    );
  }
  if let Some(recovery) = &run.recovery {
    println!("Recovery {}: {}", recovery.txid, describe(&recovery.txid)?);
  }
  Ok(())
}

fn recover(ctx: &App, args: RecoverArgs) -> Result<()> {
  let mut run = Run::load(&args.run)?;
  if run.network != ctx.network {
    bail!("the run is for {}, not {}", run.network, ctx.network);
  }
  let esplora = ctx.esplora()?;
  let wallet = ctx.wallet()?;

  let mut commits = Vec::new();
  for record in &run.reveals {
    let outpoint = record.commit_outpoint(&run.commit.txid)?;
    if esplora.tx_status(&record.tx.txid.parse()?)?.is_some() {
      continue;
    }
    if esplora.outspend(&outpoint)?.spent {
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
    println!("Every commit output is revealed or spent. Nothing to recover.");
    return Ok(());
  }

  let rate = ctx.fee_rate(args.fee_rate)?;
  let (tx, fee) = plan::build_recovery(wallet.secp(), &commits, &wallet.address(), rate)?;
  println!(
    "Recover {} commit output(s) to {}: {} sat after a fee of {} sat",
    commits.len(),
    wallet.address(),
    tx.output[0].value.to_sat(),
    fee.to_sat()
  );
  if !args.yes && !confirm("Broadcast the recovery transaction?")? {
    println!("Nothing was broadcast.");
    return Ok(());
  }
  let txid = esplora.broadcast(&tx)?;
  run.recovery = Some(run::TxRecord {
    txid: txid.to_string(),
    hex: bitcoin::consensus::encode::serialize_hex(&tx),
    fee_sat: fee.to_sat(),
    vsize: tx.vsize() as u64,
    broadcast: true,
  });
  run.save(&args.run)?;
  println!("Recovery {txid}");
  Ok(())
}
