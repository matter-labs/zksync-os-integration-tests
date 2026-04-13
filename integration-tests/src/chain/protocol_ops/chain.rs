use std::path::PathBuf;

use anyhow::Context;

use super::contracts_backend::EraContractsBackend;
use super::ProtocolOps;

const DEFAULT_SET_UPGRADE_TIMESTAMP_OUT: &str = "set_upgrade_timestamp_out.json";

pub struct ChainSetUpgradeTimestamp<'a> {
    ops: &'a ProtocolOps<'a>,
    argv: Vec<String>,
}

impl<'a> ChainSetUpgradeTimestamp<'a> {
    pub(super) fn new(ops: &'a ProtocolOps<'a>) -> Self {
        let argv = vec![
            "chain".to_string(),
            "set-upgrade-timestamp".to_string(),
            "--l1-rpc-url".to_string(),
            ops.l1_rpc_url.clone(),
        ];
        Self { ops, argv }
    }

    pub fn admin_address(mut self, v: impl AsRef<str>) -> Self {
        self.argv.push("--admin-address".to_string());
        self.argv.push(v.as_ref().to_string());
        self
    }

    pub fn new_protocol_version(mut self, v: impl AsRef<str>) -> Self {
        self.argv.push("--new-protocol-version".to_string());
        self.argv.push(v.as_ref().to_string());
        self
    }

    pub fn upgrade_timestamp(mut self, v: impl AsRef<str>) -> Self {
        self.argv.push("--upgrade-timestamp".to_string());
        self.argv.push(v.as_ref().to_string());
        self
    }

    /// Required by current `protocol_ops chain set-upgrade-timestamp`. For
    /// chains that don't have one (e.g. the v30.2 fixture), pass the zero
    /// address — the `access_control_restriction_addr` field in
    /// `contracts.yaml` already reflects that.
    pub fn access_control_restriction(mut self, v: impl AsRef<str>) -> Self {
        self.argv.push("--access-control-restriction".to_string());
        self.argv.push(v.as_ref().to_string());
        self
    }

    /// Required by current `protocol_ops chain set-upgrade-timestamp` even
    /// in `--simulate` mode. Typically the chain admin's owner (governor)
    /// private key — the same one that later signs the execute step.
    pub fn private_key(mut self, v: impl AsRef<str>) -> Self {
        self.argv.push("--private-key".to_string());
        self.argv.push(v.as_ref().to_string());
        self
    }

    pub fn build(self) -> anyhow::Result<ProtocolOpsTransactions<'a>> {
        let out_arg = self
            .ops
            .contracts_backend
            .work_path(DEFAULT_SET_UPGRADE_TIMESTAMP_OUT);

        let mut argv = self.argv;
        argv.push("--simulate".to_string());
        argv.push("--out".to_string());
        argv.push(out_arg);

        let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        self.ops
            .contracts_backend
            .protocol_ops(&argv_ref)
            .context("set-upgrade-timestamp (simulate) failed")?;

        Ok(ProtocolOpsTransactions {
            contracts_backend: self.ops.contracts_backend,
            out_relative: DEFAULT_SET_UPGRADE_TIMESTAMP_OUT.to_string(),
            l1_rpc_url: self.ops.l1_rpc_url.clone(),
        })
    }
}

pub struct ProtocolOpsTransactions<'a> {
    contracts_backend: &'a EraContractsBackend,
    out_relative: String,
    l1_rpc_url: String,
}

impl<'a> ProtocolOpsTransactions<'a> {
    /// Host-side path to the output file (for reading results back).
    pub fn out_path(&self) -> PathBuf {
        self.contracts_backend.work_dir().join(&self.out_relative)
    }

    pub fn execute_transactions(&self, private_key: impl AsRef<str>) -> anyhow::Result<()> {
        self.contracts_backend.execute_protocol_ops_out(
            &self.out_relative,
            self.l1_rpc_url.as_str(),
            private_key.as_ref(),
        )
    }
}
