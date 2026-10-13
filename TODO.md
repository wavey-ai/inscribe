# TODO

## Verification

- [ ] Do an end-to-end run on signet. The funding address needs signet coins from a faucet.
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
