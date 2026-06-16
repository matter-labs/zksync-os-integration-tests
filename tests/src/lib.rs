//! ZKsync OS integration test framework.
//!
//! # Quick start
//!
//! ```ignore
//! use tests::*;
//!
//! #[rstest]
//! #[tokio::test(flavor = "multi_thread")]
//! async fn my_test(#[future] ecosystem: Ecosystem) -> anyhow::Result<()> {
//!     let eco = ecosystem.await;
//!     let hash = eco.chain().ping().await?;
//!     eco.chain().wait_for_tx_finalized(hash).await?;
//!     Ok(())
//! }
//! ```
//!
//! See `tests/README.md` for the full API reference.

pub mod chain;
pub mod ecosystem;
pub mod eth;
pub mod fixtures;
pub mod locked_port;
pub mod server_runtime;
pub mod upgrade_v30_to_v31;
pub(crate) mod workdir;

// Types available via `use tests::*`.
pub use chain::{Chain, WALLET_KEYS};
pub use ecosystem::Ecosystem;
// Fixture functions are in `tests::fixtures`. Import them explicitly:
// `use tests::fixtures::ecosystem;`
