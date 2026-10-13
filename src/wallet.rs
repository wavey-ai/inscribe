use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use bip39::Mnemonic;
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv, Xpub};
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::All;
use bitcoin::{Address, Network, XOnlyPublicKey};

/// A single-key BIP86 taproot wallet. The key at m/86'/coin'/0'/0/0 holds
/// the funds and receives the change. The same mnemonic opens in any BIP86
/// wallet, for example Sparrow.
pub struct Wallet {
  network: Network,
  fingerprint: bitcoin::bip32::Fingerprint,
  account: Xpriv,
  keypair: Keypair,
  secp: Secp256k1<All>,
}

pub fn mnemonic_path(home: &Path) -> PathBuf {
  home.join("mnemonic")
}

pub fn coin_type(network: Network) -> u32 {
  match network {
    Network::Bitcoin => 0,
    _ => 1,
  }
}

impl Wallet {
  pub fn create(home: &Path, network: Network) -> Result<(Self, Mnemonic)> {
    let path = mnemonic_path(home);
    if path.exists() {
      bail!(
        "{} exists; the wallet already has a mnemonic",
        path.display()
      );
    }
    fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;

    let mnemonic = Mnemonic::generate(24)?;
    write_private(&path, format!("{mnemonic}\n").as_bytes())?;

    Ok((Self::from_mnemonic(&mnemonic, network)?, mnemonic))
  }

  pub fn load(home: &Path, network: Network) -> Result<Self> {
    let mnemonic = Self::load_mnemonic(home)?;
    Self::from_mnemonic(&mnemonic, network)
  }

  pub fn load_mnemonic(home: &Path) -> Result<Mnemonic> {
    let path = mnemonic_path(home);
    let words = fs::read_to_string(&path).with_context(|| {
      format!(
        "read {}; run `inscribe wallet init` to make a wallet",
        path.display()
      )
    })?;
    Ok(Mnemonic::from_str(words.trim())?)
  }

  pub fn from_mnemonic(mnemonic: &Mnemonic, network: Network) -> Result<Self> {
    let secp = Secp256k1::new();
    let master = Xpriv::new_master(network, &mnemonic.to_seed(""))?;
    let account = master.derive_priv(&secp, &account_path(network))?;
    let key = account.derive_priv(
      &secp,
      &[
        ChildNumber::from_normal_idx(0)?,
        ChildNumber::from_normal_idx(0)?,
      ],
    )?;
    let keypair = Keypair::from_secret_key(&secp, &key.private_key);

    Ok(Self {
      network,
      fingerprint: master.fingerprint(&secp),
      account,
      keypair,
      secp,
    })
  }

  pub fn network(&self) -> Network {
    self.network
  }

  pub fn internal_key(&self) -> XOnlyPublicKey {
    self.keypair.x_only_public_key().0
  }

  pub fn address(&self) -> Address {
    Address::p2tr(&self.secp, self.internal_key(), None, self.network)
  }

  /// The untweaked BIP86 key. Key-path signing tweaks it with no script tree.
  pub fn keypair(&self) -> &Keypair {
    &self.keypair
  }

  pub fn descriptor(&self) -> String {
    let xpub = Xpub::from_priv(&self.secp, &self.account);
    let coin = coin_type(self.network);
    format!("tr([{}/86h/{coin}h/0h]{xpub}/0/*)", self.fingerprint)
  }

  pub fn secp(&self) -> &Secp256k1<All> {
    &self.secp
  }
}

fn account_path(network: Network) -> DerivationPath {
  let coin = coin_type(network);
  DerivationPath::from_str(&format!("m/86'/{coin}'/0'")).expect("valid derivation path")
}

pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
  let mut options = fs::OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let mut file = options
    .open(path)
    .with_context(|| format!("create {}", path.display()))?;
  file.write_all(contents)?;
  file.sync_all()?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  // BIP86 test vector: https://github.com/bitcoin/bips/blob/master/bip-0086.mediawiki
  const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

  #[test]
  fn first_receive_address_matches_bip86_vector() {
    let wallet =
      Wallet::from_mnemonic(&Mnemonic::from_str(MNEMONIC).unwrap(), Network::Bitcoin).unwrap();
    assert_eq!(
      wallet.address().to_string(),
      "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
    );
    assert!(wallet.descriptor().starts_with("tr([73c5da0a/86h/0h/0h]xpub6BgBgsespWvERF3LHQu6CnqdvfEvtMcQjYrcRzx53QJjSxarj2afYWcLteoGVky7D3UKDP9QyrLprQ3VCECoY49yfdDEHGCtMMj92pReUsQ/0/*)"));
  }

  #[test]
  fn create_writes_a_private_mnemonic_once() {
    let dir = tempfile::tempdir().unwrap();
    let (wallet, mnemonic) = Wallet::create(dir.path(), Network::Signet).unwrap();
    assert_eq!(mnemonic.word_count(), 24);
    assert!(wallet.address().to_string().starts_with("tb1p"));
    assert!(Wallet::create(dir.path(), Network::Signet).is_err());
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode = fs::metadata(mnemonic_path(dir.path()))
        .unwrap()
        .permissions()
        .mode();
      assert_eq!(mode & 0o777, 0o600);
    }
    let loaded = Wallet::load(dir.path(), Network::Signet).unwrap();
    assert_eq!(loaded.address(), wallet.address());
  }
}
