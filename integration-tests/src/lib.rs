// ── Infrastructure: process management, ports, docker ──
pub mod infra;

// ── Chain interaction: funding, deposits, protocol_ops ──
pub mod chain;

// ── Generated l1-state.json ──
pub mod l1_state;

// ── Configuration ──
pub mod preset_paths;
pub mod presets;
pub mod server_config;
pub mod upgrade_config;

// Re-exports for backward compatibility with existing test code.
// Tests import e.g. `integration_tests::anvil::Anvil`.
pub use infra::anvil;
pub use infra::docker;
pub use infra::find_ports;
pub use infra::git_utils;
pub use infra::server;
pub use infra::utils;

pub use chain::anvil_utils;
pub use chain::l1_l2_deposit;
pub use chain::protocol_ops;
pub use chain::server_utils;
