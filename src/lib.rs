//! Bitcoin ordinal inscriptions with a local key.
//!
//! The core (`envelope`, `plan`, `run`, `wallet`, `service`, and the async
//! `api::Client`) has no file-system, clock, or OS dependency. It compiles for
//! `wasm32-unknown-unknown` and runs on Cloudflare Workers with a Fetch-based
//! [`api::Http`] backend. The `native` feature adds file storage and a reqwest
//! backend. The `cli` feature adds the `inscribe` program.

pub mod api;
pub mod envelope;
pub mod plan;
pub mod run;
pub mod service;
pub mod wallet;

pub use bitcoin;
pub use bitcoin::secp256k1::rand;

pub use api::{Client, Endpoints, Http, Response};
pub use envelope::Inscription;
pub use plan::{FeeRate, Plan};
pub use run::Run;
pub use service::{CreateOptions, FileInput};
pub use wallet::Wallet;
