use std::path::PathBuf;

pub struct AnvilConfig {
    /// TCP port to listen on. `None` → auto-pick an unused port.
    pub port: Option<u16>,
    /// EVM chain ID. Default: 31337.
    pub chain_id: u64,
    /// Block production interval in seconds. Default: 0.25.
    pub block_time_secs: f64,
    /// If set, Anvil is started with `--dump-state <path>` and
    /// `--preserve-historical-states`. `stop()` blocks until the file exists.
    pub dump_state: Option<PathBuf>,
    /// If set, Anvil is started with `--load-state <path>`.
    pub load_state: Option<PathBuf>,
}

impl Default for AnvilConfig {
    fn default() -> Self {
        Self {
            port: None,
            chain_id: 31337,
            block_time_secs: 0.25,
            dump_state: None,
            load_state: None,
        }
    }
}
