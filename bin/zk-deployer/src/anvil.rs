use std::io::Read as _;
use std::path::Path;

use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::primitives::Bytes;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use flate2::read::GzDecoder;

/// Builder for the deploy phase (bootstrap/apply). Pure automine — one block
/// per submitted transaction, no interval-mined empty blocks — so the dumped
/// L1 state stays compact. `anvil_dumpState(true)` still preserves historical
/// states here, so a restored dump can serve historical L1 reads.
pub fn deploy_builder() -> Anvil {
    Anvil::new().args(["--slots-in-an-epoch", "2"])
}

/// Builder for the run phase (serving a restored chain). Interval mining keeps
/// L1 advancing in wall-clock time so the sequencer's finality-gated pipeline
/// (commit → prove → execute) makes progress without new user transactions.
pub fn default_builder() -> Anvil {
    Anvil::new()
        .block_time_f64(0.25)
        .arg("--mixed-mining")
        .args(["--slots-in-an-epoch", "2"])
}

pub async fn spawn(builder: Anvil) -> Result<AnvilInstance> {
    tokio::task::spawn_blocking(move || builder.try_spawn())
        .await
        .context("spawn_blocking panicked")?
        .context("anvil spawn failed — is 'anvil' installed and in PATH?")
}

pub async fn spawn_from_file(builder: Anvil, path: &Path) -> Result<AnvilInstance> {
    // anvil's --load-state only accepts plain UTF-8 JSON (SerializableState).
    // Two .gz formats are in use; we detect and peel all layers here.
    let is_gz = path.extension().map(|e| e == "gz").unwrap_or(false);
    if is_gz {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read gz state file: {}", path.display()))?;
        let decompressed = decompress_gzip(&bytes)
            .with_context(|| format!("decompress outer gz: {}", path.display()))?;
        // Two formats are in use for .gz fixtures:
        //   Format A: gzip(state_json)          — plain JSON state, used by save_state
        //   Format B: gzip("0x" + hex(gzip(state_json))) — produced by cast rpc anvil_dumpState
        // Detect by whether the decompressed content starts with `"0x`.
        let state_json = if decompressed.starts_with(b"\"0x") {
            let inner_str = std::str::from_utf8(&decompressed)
                .context("gz state inner content is not valid UTF-8")?
                .trim();
            let hex_encoded = inner_str
                .strip_prefix("\"0x")
                .and_then(|s| s.strip_suffix('"'))
                .with_context(|| "gz state inner content: expected JSON hex string")?;
            let raw = alloy::hex::decode(hex_encoded).context("hex-decode inner state")?;
            decompress_gzip(&raw).context("decompress inner gz state")?
        } else {
            decompressed
        };
        let mut tmp = tempfile::NamedTempFile::new().context("create temp state file")?;
        std::io::Write::write_all(&mut tmp, &state_json).context("write state")?;
        let tmp_path = tmp.path().to_path_buf();
        let instance = spawn(builder.arg("--load-state").arg(&tmp_path)).await?;
        // Keep tmp alive until after spawn completes (anvil reads the file at startup).
        drop(tmp);
        return Ok(instance);
    }
    let path_str = path
        .to_str()
        .with_context(|| format!("state path is not valid UTF-8: {}", path.display()))?;
    spawn(builder.arg("--load-state").arg(path_str)).await
}

pub async fn save_state(instance: &AnvilInstance, path: &Path) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect(&instance.endpoint())
        .await
        .context("connect to anvil")?;
    let bytes: Bytes = provider
        .client()
        .request::<(bool,), Bytes>("anvil_dumpState", (true,))
        .await
        .context("anvil_dumpState")?;
    // anvil_dumpState returns gzip-compressed JSON; decompress so that
    // `--load-state` (which expects plain UTF-8 JSON) can read the file.
    let json = decompress_gzip(&bytes).context("decompress anvil state")?;
    tokio::fs::write(path, &json)
        .await
        .with_context(|| format!("write state to {}", path.display()))
}

fn decompress_gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

/// Resolve the L1 RPC URL. If `configured` is `Some`, return it unchanged.
/// If `None`, spawn a managed Anvil — restoring from `state_path` if it exists,
/// otherwise starting fresh. The returned `AnvilInstance` must be held alive
/// by the caller for the duration of the command.
pub async fn resolve_l1(
    configured: Option<&str>,
    state_path: &Path,
) -> Result<(String, Option<AnvilInstance>)> {
    if let Some(url) = configured {
        return Ok((url.to_string(), None));
    }
    let instance = if state_path.exists() {
        spawn_from_file(deploy_builder(), state_path).await?
    } else {
        spawn(deploy_builder()).await?
    };
    let url = instance.endpoint();
    Ok((url, Some(instance)))
}
