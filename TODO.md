# TODO

## Verification

- [x] Do an end-to-end run on signet. The Worker under `wrangler dev` created, stored, and broadcast run `1791889579-775a85035bbb289f` (commit `775a85035bbb289fb55e20a823d84ffb7d5d165128a4b6ff1e8eaac88964280d`, inscription `23b297437904b5e1202bd505297fbe3c28e6a0a1a488d7de10bd4bc9a78b3fa9i0`).
- [ ] Do a first mainnet run with one small file at a low fee rate. Make sure that ordinals.com shows the inscription with the correct content type.

## Inscription features

- [ ] Parent and child inscriptions (tag 3), so that each release has a provenance parent. The reveal must also spend the parent inscription.
- [ ] Metadata (tag 5, CBOR) for release information.
- [ ] Delegate inscriptions (tag 11) for copies of one content item.
- [ ] Batch reveals: more than one envelope in one reveal transaction, with pointers (tag 2). This reduces the overhead for small files.
- [ ] Content split across more than one inscription for bodies above approximately 395 KB, with a loader that joins the parts through `/content/<id>`.

## Wallet

- [ ] Encryption of the mnemonic file with a passphrase.
- [ ] A watch-only mode, so that a different machine holds the key and signs a PSBT.
- [ ] A new change address for each run.

## Fees and broadcast

- [ ] Fee bumps for a stuck reveal (RBF or CPFP).
- [ ] A second Esplora endpoint as a fallback for the broadcast.
- [ ] Runs of more than 24 files, with a commit transaction for each group of 24.

## Worker

- [ ] A lock for run creation, for example a Durable Object, so that two runs cannot select the same funding output.
- [ ] Multipart uploads as an alternative to base64 JSON bodies.
- [ ] A deployment with a route on a wavey.ai domain.
