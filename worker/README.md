# inscribe-worker

`inscribe-worker` is a Cloudflare Worker that exposes the `inscribe` library as a JSON API. It holds the funding mnemonic as a Worker secret. It stores each run in Workers KV before it broadcasts any transaction.

## Bindings

| Binding | Type | Description |
|---|---|---|
| `INSCRIBE_MNEMONIC` | Secret | The BIP39 mnemonic of the funding wallet |
| `INSCRIBE_API_TOKEN` | Secret | The bearer token for every request |
| `INSCRIBE_NETWORK` | Variable | `bitcoin`, `signet`, or `testnet4`. The default is `bitcoin`. |
| `INSCRIBE_ESPLORA_URL` | Variable (optional) | The Esplora endpoint. The default is mempool.space for the network. |
| `INSCRIBE_ORD_URL` | Variable (optional) | The `ord` server. The default on mainnet is `https://ordinals.com`. |
| `INSCRIBE_RUNS` | KV namespace | The run records. Each record holds the keys that recover the commit outputs. |

On mainnet, the Worker checks each funding output with the `ord` server. On the test networks, the Worker does not do this check.

## Routes

Each request must have the header `Authorization: Bearer <INSCRIBE_API_TOKEN>`. The Worker returns HTTP 401 for a request without a valid token.

| Method and path | Description |
|---|---|
| `GET /v1/wallet` | The network, the funding address, the descriptor, and the funding outputs |
| `GET /v1/fees` | The mempool.space recommended fee rates |
| `POST /v1/estimate` | The size and the network fee of each reveal and of the commit |
| `POST /v1/runs` | Builds, signs, and stores a run. With `"broadcast": true`, the Worker also broadcasts it. |
| `GET /v1/runs/<id>` | The run without its keys, and the state of each transaction |
| `POST /v1/runs/<id>/broadcast` | Broadcasts the transactions that the network does not have. A repeat call is safe. |
| `POST /v1/runs/<id>/recover` | Returns the unrevealed commit outputs to the funding address |

### Request body for estimate and runs

```json
{
  "files": [
    { "name": "track1.png", "body_base64": "iVBORw0KGgo..." },
    { "name": "player.html", "body_base64": "PCFkb2N0eXBl...", "content_type": "text/html;charset=utf-8" }
  ],
  "compress": true,
  "postage": 546,
  "fee_rate": 2,
  "commit_fee_rate": 2,
  "metaprotocol": null,
  "destination": "bc1p...",
  "include_unconfirmed": false,
  "broadcast": false
}
```

- `files` is necessary. The extension of `name` sets the content type when `content_type` is not set.
- `destination` is necessary for `POST /v1/runs` only.
- When `fee_rate` is not set, the Worker uses the mempool.space "half hour" rate.
- The response of `POST /v1/runs` contains the run `id`, the run without its keys, the skipped funding outputs, and the total network fee.

### Request body for recover

```json
{ "fee_rate": 2 }
```

## Build

The build needs `worker-build` and a C compiler that targets WebAssembly. Refer to "WebAssembly build" in the root `README.md`.

```sh
export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar
worker-build --release --no-panic-recovery
```

The build writes `build/worker/shim.mjs` and `build/worker/index.wasm`. The WebAssembly module is approximately 2.9 MB, or 1.9 MB with gzip.

## Local test

1. Make a `.dev.vars` file:

   ```sh
   INSCRIBE_MNEMONIC="<a signet test mnemonic>"
   INSCRIBE_API_TOKEN="<a local token>"
   ```

2. Start the Worker:

   ```sh
   npm install
   npm run dev
   ```

3. Send a request:

   ```sh
   curl -H "Authorization: Bearer <a local token>" http://127.0.0.1:8787/v1/wallet
   ```

## Deployment

1. Make the KV namespace, and put its ID in `wrangler.jsonc`:

   ```sh
   npx wrangler kv namespace create INSCRIBE_RUNS
   ```

2. Set the secrets:

   ```sh
   npx wrangler secret put INSCRIBE_MNEMONIC
   npx wrangler secret put INSCRIBE_API_TOKEN
   ```

3. Set `INSCRIBE_NETWORK` in `wrangler.jsonc`. The file sets `signet`.
4. Deploy:

   ```sh
   npm run deploy
   ```

`wrangler.jsonc` sets `workers_dev` to false and has no route. Add a route or a custom domain before the deployment.

## CPU time

`wrangler.jsonc` sets a CPU limit of 30 seconds. Brotli compression at quality 11 uses the most CPU time. A Worker on the Free plan has a CPU limit of 10 ms, which is too small for compression.

## Concurrency

Two `POST /v1/runs` requests at the same time can select the same funding output. The network then rejects the second commit transaction. Send one run at a time.
