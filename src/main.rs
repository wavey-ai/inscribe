use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Amount, Network};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rand_core::OsRng;

use inscribe::api::{Client, Endpoints, ReqwestHttp};
use inscribe::plan::{self, FeeRate};
use inscribe::run::Run;
use inscribe::service::{self, CreateOptions, FileInput, SkipReason, TxState};
use inscribe::wallet::{Wallet, store};

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

  fn name(self) -> &'static str {
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

impl FeeArgs {
  fn reveal(&self) -> Result<Option<FeeRate>> {
    self.fee_rate.map(FeeRate::new).transpose()
  }

  fn commit(&self) -> Result<Option<FeeRate>> {
    self.commit_fee_rate.map(FeeRate::new).transpose()
  }
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
  chain: Chain,
  network: Network,
  home: PathBuf,
  client: Option<Client<ReqwestHttp>>,
}

impl App {
  fn new(cli: &Cli) -> Result<Self> {
    let network = cli.chain_network();
    let root = match &cli.home {
      Some(home) => home.clone(),
      None => std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".inscribe"))
        .ok_or_else(|| anyhow!("HOME is not set; use --home"))?,
    };
    let defaults = Endpoints::defaults(network);
    let esplora = cli
      .esplora_url
      .clone()
      .or_else(|| defaults.as_ref().map(|endpoints| endpoints.esplora.clone()));
    let ord = cli.ord_url.clone().or_else(|| {
      defaults
        .as_ref()
        .and_then(|endpoints| endpoints.ord.clone())
    });
    let client = match esplora {
      Some(esplora) => Some(Client::new(ReqwestHttp::new()?, Endpoints { esplora, ord })),
      None => None,
    };
    Ok(Self {
      chain: cli.network,
      network,
      home: root.join(cli.network.name()),
      client,
    })
  }

  fn client(&self) -> Result<&Client<ReqwestHttp>> {
    self
      .client
      .as_ref()
      .ok_or_else(|| anyhow!("no Esplora endpoint for this network; use --esplora-url"))
  }

  fn runs_dir(&self) -> PathBuf {
    self.home.join("runs")
  }

  fn wallet(&self) -> Result<Wallet> {
    store::load(&self.home, self.network)
  }

  fn fiat(&self, amount: Amount) -> String {
    if self.network != Network::Bitcoin {
      return String::new();
    }
    match self.client.as_ref().map(|client| block_on(client.prices())) {
      Some(Ok(prices)) => {
        let btc = amount.to_btc();
        format!(" (£{:.2}, ${:.2})", btc * prices.gbp, btc * prices.usd)
      }
      _ => String::new(),
    }
  }

  fn load_run(&self, path: &Path) -> Result<Run> {
    let run = Run::load(path)?;
    if run.network != self.network {
      bail!("the run is for {}, not {}", run.network, self.network);
    }
    Ok(run)
  }
}

impl Cli {
  fn chain_network(&self) -> Network {
    self.network.network()
  }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
  pollster::block_on(future)
}

fn main() {
  if let Err(err) = run_cli() {
    eprintln!("error: {err:#}");
    std::process::exit(1);
  }
}

fn run_cli() -> Result<()> {
  let cli = Cli::parse();
  let app = App::new(&cli)?;

  match cli.command {
    Command::Wallet(command) => wallet_command(&app, command),
    Command::Estimate(args) => estimate(&app, args),
    Command::Create(args) => create(&app, args),
    Command::Broadcast { run } => broadcast(&app, &run),
    Command::Status { run } => status(&app, &run),
    Command::Recover(args) => recover(&app, args),
  }
}

fn wallet_command(app: &App, command: WalletCommand) -> Result<()> {
  match command {
    WalletCommand::Init => {
      let (wallet, _mnemonic) = store::create(&app.home, app.network)?;
      println!(
        "Saved a new mnemonic in {}",
        store::mnemonic_path(&app.home).display()
      );
      println!(
        "Back up the mnemonic now: inscribe --network {} wallet mnemonic",
        app.chain.name()
      );
      println!("Funding address: {}", wallet.address());
      println!("Descriptor: {}", wallet.descriptor());
    }
    WalletCommand::Address => println!("{}", app.wallet()?.address()),
    WalletCommand::Descriptor => println!("{}", app.wallet()?.descriptor()),
    WalletCommand::Mnemonic => println!("{}", store::load_mnemonic(&app.home)?),
    WalletCommand::Balance => {
      let wallet = app.wallet()?;
      let utxos = block_on(app.client()?.utxos(&wallet.address()))?;
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
      println!("Confirmed: {confirmed}{}", app.fiat(confirmed));
      println!("Unconfirmed: {unconfirmed}");
    }
  }
  Ok(())
}

fn read_files(args: &ContentArgs) -> Result<service::Prepared> {
  let files = args
    .files
    .iter()
    .map(|path| {
      Ok(FileInput {
        name: path.display().to_string(),
        bytes: std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        content_type: args.content_type.clone(),
      })
    })
    .collect::<Result<Vec<_>>>()?;
  service::prepare(files, args.compress, args.metaprotocol.as_deref())
}

fn estimate(app: &App, args: EstimateArgs) -> Result<()> {
  let prepared = read_files(&args.content)?;
  let reveal_rate = match args.fees.reveal()? {
    Some(rate) => rate,
    None => block_on(service::fee_rate(app.client()?, None))?,
  };
  let commit_rate = args.fees.commit()?.unwrap_or(reveal_rate);
  let secp = bitcoin::secp256k1::Secp256k1::new();
  let estimate = plan::estimate(
    &secp,
    prepared.inscriptions,
    Amount::from_sat(args.content.postage),
    commit_rate,
    reveal_rate,
  )?;

  println!(
    "Reveal fee rate {} sat/vB, commit fee rate {} sat/vB",
    reveal_rate.sat_per_vb(),
    commit_rate.sat_per_vb()
  );
  for (reveal, name) in estimate.reveals.iter().zip(&prepared.names) {
    println!(
      "{name}: {} bytes{}, reveal {} vB, fee {} sat",
      reveal.body_bytes,
      if reveal.compressed { " (brotli)" } else { "" },
      reveal.vsize,
      reveal.fee_sat
    );
  }
  println!(
    "Commit: {} vB, fee {} sat",
    estimate.commit_vsize, estimate.commit_fee_sat
  );
  let fees = Amount::from_sat(estimate.total_fee_sat);
  println!("Network fees: {} sat{}", fees.to_sat(), app.fiat(fees));
  println!(
    "Postage held in the inscriptions: {} sat",
    estimate.total_postage_sat
  );
  Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
  print!("{prompt} Type yes to continue: ");
  io::stdout().flush()?;
  let mut line = String::new();
  io::stdin().lock().read_line(&mut line)?;
  Ok(line.trim() == "yes")
}

fn now_unix() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_secs())
    .unwrap_or_default()
}

fn create(app: &App, args: CreateArgs) -> Result<()> {
  let wallet = app.wallet()?;
  let destination = args
    .destination
    .clone()
    .require_network(app.network)
    .context("the destination address is for a different network")?;
  let prepared = read_files(&args.content)?;
  let options = CreateOptions {
    destination: destination.clone(),
    postage: Amount::from_sat(args.content.postage),
    reveal_fee_rate: args.fees.reveal()?,
    commit_fee_rate: args.fees.commit()?,
    include_unconfirmed: args.include_unconfirmed,
    ord_check: !args.no_ord_check,
  };
  let created = block_on(service::create(
    app.client()?,
    &wallet,
    prepared,
    &options,
    now_unix(),
    &mut OsRng,
  ))?;
  for skipped in &created.skipped {
    let reason = match skipped.reason {
      SkipReason::Unconfirmed => "no confirmation",
      SkipReason::HoldsInscriptionOrRunes => "it holds an inscription or runes",
      SkipReason::NotIndexed => "the ord server has not indexed it",
    };
    eprintln!("skip {}: {reason}", skipped.outpoint);
  }

  let run = created.run;
  let path = run.default_path(&app.runs_dir());
  run.save(&path)?;

  println!("Network: {}", app.chain.name());
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
  if let Some(change) = created.plan.change {
    println!("Change to the wallet: {} sat", change.to_sat());
  }
  let fees = created.plan.total_spent();
  println!("Network fees: {} sat{}", fees.to_sat(), app.fiat(fees));
  println!(
    "Postage held in the inscriptions: {} sat",
    created.plan.total_postage().to_sat()
  );
  println!("Run file: {}", path.display());

  if args.dry_run {
    println!(
      "Dry run: nothing was broadcast. To broadcast later: inscribe --network {} broadcast {}",
      app.chain.name(),
      path.display()
    );
    return Ok(());
  }
  if !args.yes && !confirm("Broadcast these transactions?")? {
    println!("Nothing was broadcast. The run file stays for a later broadcast.");
    return Ok(());
  }
  broadcast(app, &path)
}

fn broadcast(app: &App, path: &Path) -> Result<()> {
  let mut run = app.load_run(path)?;
  let result = block_on(service::broadcast(app.client()?, &mut run));
  run.save(path)?;
  let report = result?;
  println!("Commit {} is in the network", report.commit_txid);
  for id in &report.revealed {
    println!("Reveal {id}");
  }
  for failure in &report.failures {
    eprintln!("Reveal {} failed: {}", failure.file, failure.error);
  }
  if !report.failures.is_empty() {
    bail!(
      "{} reveal(s) failed. Run `broadcast` again, or `recover` to return the unrevealed commit outputs to the wallet.",
      report.failures.len()
    );
  }
  Ok(())
}

fn describe(state: &TxState) -> String {
  match state {
    TxState::NotInNetwork => "not in the network".into(),
    TxState::Mempool => "in the mempool".into(),
    TxState::Confirmed { height } => format!("confirmed at height {height}"),
  }
}

fn status(app: &App, path: &Path) -> Result<()> {
  let run = app.load_run(path)?;
  let status = block_on(service::status(app.client()?, &run))?;
  println!(
    "Commit {}: {}",
    status.commit_txid,
    describe(&status.commit)
  );
  for reveal in &status.reveals {
    println!(
      "{} {}: {}",
      reveal.inscription_id,
      reveal.file,
      describe(&reveal.state)
    );
  }
  if let Some(state) = &status.recovery {
    println!("Recovery: {}", describe(state));
  }
  Ok(())
}

fn recover(app: &App, args: RecoverArgs) -> Result<()> {
  let mut run = app.load_run(&args.run)?;
  let wallet = app.wallet()?;
  let client = app.client()?;
  let rate = args.fee_rate.map(FeeRate::new).transpose()?;
  let Some(recovery) = block_on(service::recovery(client, &wallet, &run, rate, &mut OsRng))? else {
    println!("Every commit output is revealed or spent. Nothing to recover.");
    return Ok(());
  };
  println!(
    "Recover {} commit output(s) to {}: {} sat after a fee of {} sat",
    recovery.outputs.len(),
    wallet.address(),
    recovery.tx.output[0].value.to_sat(),
    recovery.fee.to_sat()
  );
  if !args.yes && !confirm("Broadcast the recovery transaction?")? {
    println!("Nothing was broadcast.");
    return Ok(());
  }
  let txid = block_on(service::broadcast_recovery(client, &mut run, &recovery))?;
  run.save(&args.run)?;
  println!("Recovery {txid}");
  Ok(())
}
