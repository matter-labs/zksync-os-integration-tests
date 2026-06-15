mod wait;

pub use wait::{wait_for_l2_block_finalized, wait_for_l2_block_produced, wait_for_rpc_ready};

#[cfg(feature = "embedded-server")]
mod embedded;
#[cfg(feature = "embedded-server")]
pub use embedded::{load_config_from_yaml, Server};
#[cfg(feature = "embedded-server")]
pub use zksync_os_server::config::{Config, ExternalPriceApiClientConfig, ForcedPriceClientConfig};
