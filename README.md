# inscribe

`inscribe` is a Rust library and a command-line tool that create Bitcoin ordinal inscriptions. It holds its own key, and it builds and signs the commit and reveal transactions locally. It uses public endpoints for chain data and for the broadcast. It does not use a Bitcoin node or an inscription service.

The library core compiles for `wasm32-unknown-unknown` and runs on Cloudflare Workers. The `worker/` directory holds a Worker that exposes the library as a JSON API.

## Terms

- **Inscription**: content in a taproot script-path witness. The `ord` indexer assigns the content to the first satoshi of the reveal input.
- **Commit transaction**: the transaction that pays to one taproot output for each inscription. The script tree of each output holds the inscription envelope.
- **Reveal transaction**: the transaction that spends one commit output through the script path. This spend puts the envelope on the chain.
- **Postage**: the value of the output that holds an inscription. The default is 546 sat.
- **Run**: one commit transaction and its reveal transactions. A run record holds the signed transactions and the keys that recover the commit outputs.
- **Esplora**: the REST interface of mempool.space and of Blockstream Esplora servers.

## Compatibility

The envelope follows `ord` 0.29.0: `OP_FALSE OP_IF "ord"`, then the content type (tag 1), the content encoding (tag 9), the metaprotocol (tag 7), an empty push, the body in 520-byte pushes, and `OP_ENDIF`. The content-type table and the brotli parameters are copies of the `ord` 0.29.0 values.

The test suite compares the tapscripts of two mainnet inscriptions with the output of this library. The two tapscripts are byte-identical:

- `16695e97fc2425f4ef6d23cb4aafb473c9ebd699a50adea94624a6cd15ba29e1i0` (HTML, block 850303)
- `4bdd3d72da1fe299ed89cd2c71b29b901807d1b3d41d447aec8a5383eacddf84i0` (PNG, block 849187)

The test suite also verifies each commit, reveal, and recovery input with `libbitcoinconsensus` (Bitcoin Core 26.0) with the taproot rules on.

## Library

### Features

| Feature | Default | Contents |
|---|---|---|
| (none) | — | The core: envelope, planner, run record, wallet, async service layer, and async chain client. The core compiles for `wasm32-unknown-unknown`. |
| `native` | yes | Mnemonic and run-file storage, and `ReqwestHttp`, a blocking reqwest backend for the chain client. |
| `cli` | yes | The `inscribe` program. `cli` turns on `native`. |

For a Worker or another WebAssembly host, use the core only:

```toml
inscribe = { git = "https://github.com/wavey-ai/inscribe", default-features = false }
```

### Modules

| Module | Contents |
|---|---|
| `envelope` | `Inscription`, the envelope script, the content-type table, and the brotli compression |
| `plan` | `plan::build` (commit and reveals, coin selection, signatures), `plan::estimate`, and `plan::build_recovery` |
| `wallet` | `Wallet` (BIP39 and BIP86) and `generate_mnemonic`. With `native`: `wallet::store` |
| `run` | `Run`, the serializable run record |
| `api` | `Client`, the async Esplora and `ord` client, and the `Http` trait |
| `service` | `prepare`, `create`, `broadcast`, `status`, `recovery`, and `broadcast_recovery` |

### Host requirements

The core takes three things from the caller:

- **HTTP**: an implementation of `api::Http`. The trait has two async methods, `get` and `post`. The Worker implements it with `worker::Fetch`. Native programs use `api::ReqwestHttp`.
- **Time**: `service::create` takes the creation time in seconds since the Unix epoch.
- **Randomness**: each signing function takes an RNG that implements `rand::Rng + rand::CryptoRng`. Use `rand_core::OsRng`. On `wasm32-unknown-unknown`, `OsRng` uses `crypto.getRandomValues` through the `getrandom` "js" backend.

### Example

```rust
use inscribe::api::{Client, Endpoints};
use inscribe::bitcoin::{Amount, Network};
use inscribe::service::{self, CreateOptions, FileInput};
use inscribe::wallet::Wallet;

async fn release<H: inscribe::Http>(http: H, phrase: &str, files: Vec<FileInput>, now: u64) -> anyhow::Result<()> {
  let network = Network::Bitcoin;
  let wallet = Wallet::from_phrase(phrase, network)?;
  let client = Client::new(http, Endpoints::defaults(network).unwrap());

  let prepared = service::prepare(files, true, None)?;
  let options = CreateOptions {
    destination: wallet.address(),
    postage: Amount::from_sat(546),
    reveal_fee_rate: None, // the mempool.space "half hour" rate
    commit_fee_rate: None,
    include_unconfirmed: false,
    ord_check: true,
  };
  let mut run = service::create(&client, &wallet, prepared, &options, now, &mut rand_core::OsRng)
    .await?
    .run;

  // Store `run` here. It holds the keys that recover the commit outputs.

  let report = service::broadcast(&client, &mut run).await?;
  // Store `run` again: the broadcast flags changed.
  println!("{:?}", report.revealed);
  Ok(())
}
```

### WebAssembly build

The `secp256k1` dependency contains C code. A build for `wasm32-unknown-unknown` needs a C compiler that targets WebAssembly. Apple clang has no WebAssembly target. Use one of these compilers:

- LLVM clang, for example from Homebrew (`brew install llvm`) or from the Linux distribution:

  ```sh
  export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
  export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar
  ```

- `zig cc` through a wrapper script that replaces `--target=wasm32-unknown-unknown` with `-target wasm32-freestanding`.

Then build the core:

```sh
cargo build --lib --no-default-features --target wasm32-unknown-unknown --release
```

## Cloudflare Worker

`worker/` holds a Worker that exposes the library as an authenticated JSON API. Refer to `worker/README.md` for the bindings, the routes, and the deployment procedure.

## Command-line program

### Requirements

- Rust 1.85 or later.
- Network access to an Esplora endpoint. The defaults are `https://mempool.space/api`, `https://mempool.space/signet/api`, and `https://mempool.space/testnet4/api`.
- On mainnet, network access to an `ord` server. The default is `https://ordinals.com`. The program uses the recursive endpoint `/r/utxo/<outpoint>`.

### Installation

```sh
cargo install --path .
```

### Data directory

The default data directory is `~/.inscribe`. Set `--home` or `INSCRIBE_HOME` to use a different directory. Each network has a subdirectory: `bitcoin`, `signet`, `testnet4`, or `regtest`.

| Path | Contents |
|---|---|
| `<home>/<network>/mnemonic` | The 24-word BIP39 mnemonic of the wallet. The file mode is 0600. |
| `<home>/<network>/runs/<time>-<commit>.json` | One file for each run. The file mode is 0600. The file holds the ephemeral secret keys of the commit outputs. |

Back up the mnemonic. Keep each run file until all its reveals have confirmations.

### Wallet

The wallet holds one BIP86 key at `m/86'/0'/0'/0/0` (mainnet) or `m/86'/1'/0'/0/0` (test networks). The funds and the change use the address of this key. The mnemonic opens in other BIP86 wallets, for example Sparrow.

```sh
inscribe wallet init          # Makes and saves a new mnemonic
inscribe wallet mnemonic      # Shows the mnemonic
inscribe wallet address       # Shows the funding address
inscribe wallet balance       # Shows the funding outputs
inscribe wallet descriptor    # Shows the output descriptor for a watch-only wallet
```

Send only plain bitcoin to the funding address. Do not send inscriptions or runes to it.

### Cost estimate

`estimate` shows the size and the network fee of each reveal and of the commit. It needs no wallet.

```sh
inscribe estimate track1.png track2.png player.html --compress --fee-rate 2
```

If you do not set `--fee-rate`, the program uses the mempool.space "half hour" rate. On mainnet, the program also shows the cost in GBP and USD.

### Inscription procedure

1. Make a wallet on signet:

   ```sh
   inscribe --network signet wallet init
   ```

2. Send signet coins to the funding address from a signet faucet.
3. Make a test run on signet:

   ```sh
   inscribe --network signet create cover.png player.html \
     --destination <signet taproot address> --compress --no-ord-check
   ```

4. Make sure that each reveal has a confirmation:

   ```sh
   inscribe --network signet status ~/.inscribe/signet/runs/<run>.json
   ```

5. Make a wallet on mainnet and fund it:

   ```sh
   inscribe wallet init
   inscribe wallet address
   ```

6. Make a dry run on mainnet:

   ```sh
   inscribe create track1.png track2.png player.html \
     --destination <your ordinals address> --compress --dry-run
   ```

   The program shows the inscription IDs, the fees, and the path of the run file. It broadcasts nothing.

7. Broadcast the run:

   ```sh
   inscribe broadcast ~/.inscribe/bitcoin/runs/<run>.json
   ```

   Alternatively, use `create` without `--dry-run`. The program then asks for confirmation before the broadcast.

8. Monitor the run until all reveals have confirmations:

   ```sh
   inscribe status ~/.inscribe/bitcoin/runs/<run>.json
   ```

The inscription ID of each file is `<reveal txid>i0`. The IDs are known before the broadcast. An HTML file that loads other inscriptions through `/content/<id>` needs the IDs of those inscriptions. Inscribe the referenced files in an earlier run, because the ID of each reveal depends on all files in its run.

### Content types

The program sets the content type from the file extension with the `ord` table, for example:

| Extension | Content type |
|---|---|
| `.html` | `text/html;charset=utf-8` |
| `.js` | `text/javascript` |
| `.png` | `image/png` |
| `.opus` | `audio/ogg;codecs=opus` |
| `.flac` | `audio/flac` |
| `.mp3` | `audio/mpeg` |

Use `--content-type` to set one content type for all files in a command. Use `--compress` to compress each body with brotli. The program keeps the compressed body only if it is smaller, and then sets the content encoding to `br`.

### Funding outputs

On mainnet, the program asks the `ord` server about each funding output before it spends the output. The program does not spend an output that holds an inscription or runes, or an output that the `ord` server has not indexed. The program also does not spend unconfirmed outputs. Use `--include-unconfirmed` to spend unconfirmed outputs. Use `--no-ord-check` on test networks that have no `ord` server.

The coin selection uses the largest outputs first. The change goes back to the funding address. If the change is below the dust limit, the change goes to the miner.

### Recovery

If a reveal does not reach the network, run `broadcast` again. If a reveal cannot confirm, `recover` sends the unrevealed commit outputs back to the funding address:

```sh
inscribe recover ~/.inscribe/bitcoin/runs/<run>.json --fee-rate 2
```

`recover` spends only the commit outputs whose reveal is not in the network and whose output is unspent. It uses the key path of each commit output. The key path uses the ephemeral key from the run file.

### Options

| Option | Environment variable | Description |
|---|---|---|
| `--network` | `INSCRIBE_NETWORK` | `bitcoin` (default), `signet`, `testnet4`, or `regtest` |
| `--home` | `INSCRIBE_HOME` | The data directory |
| `--esplora-url` | `INSCRIBE_ESPLORA_URL` | The Esplora endpoint |
| `--ord-url` | `INSCRIBE_ORD_URL` | The `ord` server for the funding-output check |

## Limits

- One run holds a maximum of 24 files. The default mempool policy accepts a chain of 25 unconfirmed transactions: one commit and 24 reveals.
- One reveal transaction has a maximum weight of 400,000 WU. This limit sets the maximum body size to approximately 395 KB. Split larger content into more inscriptions.
- The fee rate can be a fraction, for example `1.5` sat/vB.

## Tests

```sh
cargo test
```

The tests use no network access. `tests/service.rs` runs the async service layer against an in-memory Esplora and `ord` server.

## Source layout

| File | Contents |
|---|---|
| `src/envelope.rs` | The inscription envelope, the content-type table, and the brotli compression |
| `src/plan.rs` | The commit and reveal construction, the fee calculation, the coin selection, the signatures, and the estimate |
| `src/wallet.rs` | The BIP39 and BIP86 wallet, and the mnemonic storage (`native`) |
| `src/api.rs` | The async Esplora and `ord` client, the `Http` trait, and the reqwest backend (`native`) |
| `src/service.rs` | The workflow: funding selection, run creation, broadcast, status, and recovery |
| `src/run.rs` | The run record |
| `src/main.rs` | The command-line program (`cli`) |
| `tests/compat.rs` | The mainnet compatibility tests and the consensus tests |
| `tests/service.rs` | The service-layer tests with a mock HTTP backend |
| `worker/` | The Cloudflare Worker |

## License

MIT. Refer to `LICENSE`.
