use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RunJson {
    transactions: Vec<RunTx>,
}

#[derive(Debug, Deserialize)]
struct RunTx {
    hash: String,
}

pub fn generate_upgrade_yaml_output(
    run_file_path: &Path,
    output_toml_path: &Path,
    yaml_output_path: &Path,
) -> anyhow::Result<()> {
    let run_bytes = std::fs::read(run_file_path)
        .with_context(|| format!("Failed to read {}", run_file_path.display()))?;
    let run_json: RunJson = serde_json::from_slice(&run_bytes)
        .with_context(|| format!("Failed to parse JSON {}", run_file_path.display()))?;

    let tx_hashes: Vec<toml::Value> = run_json
        .transactions
        .into_iter()
        .map(|tx| toml::Value::String(tx.hash))
        .collect();

    let toml_str = std::fs::read_to_string(output_toml_path)
        .with_context(|| format!("Failed to read {}", output_toml_path.display()))?;
    let mut v: toml::Value = toml::from_str(&toml_str)
        .with_context(|| format!("Failed to parse TOML {}", output_toml_path.display()))?;

    let table = v
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("TOML root is not a table: {}", output_toml_path.display()))?;
    table.insert("transactions".to_string(), toml::Value::Array(tx_hashes));

    let yaml = serde_yaml::to_string(&v).with_context(|| "Failed to serialize YAML")?;
    if let Some(parent) = yaml_output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    std::fs::write(yaml_output_path, yaml)
        .with_context(|| format!("Failed to write {}", yaml_output_path.display()))?;

    Ok(())
}





