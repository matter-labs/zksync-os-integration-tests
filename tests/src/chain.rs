use std::sync::Mutex;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context as _, Result};

use crate::activity::{
    deposit_wallet_index, run_l1_deposits, run_l2_transfers, self_transfer, transfer_wallet_index,
    ActivityConfig, ActivityHandle, ActivityReport, ActivityState, ACTIVITY_WALLET_KEYS,
};

/// Pre-funded test wallets, each rich on L2 after setup. These are the same
/// well-known dev accounts that `zk-deployer` funds by default on local/Anvil
/// deployments — re-exported here so the canonical set lives in one place.
///
/// - `WALLET_KEYS[0]`: 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049 (ZKsync rich account)
/// - `WALLET_KEYS[1]`: 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 (Anvil #1)
/// - `WALLET_KEYS[2]`: 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC (Anvil #2)
/// - `WALLET_KEYS[3..]`: Anvil #3–#9
pub use zk_deployer::l1_l2_deposit::DEFAULT_L2_RICH_KEYS as WALLET_KEYS;

/// Default timeout for waiting on L2 block *production* (sealing). Sealing is
/// fast even under load, so this is generous headroom rather than a tight bound.
pub const BLOCK_PRODUCED_TIMEOUT: Duration = Duration::from_secs(120);

/// Default timeout for waiting on L2 block *finalization* (commit → prove →
/// execute on L1). Generous because tests run in parallel: several in-process
/// servers plus their prover pipelines share one machine, and batch cycles
/// stretch well past 120 s under that contention.
pub const BLOCK_FINALIZED_TIMEOUT: Duration = Duration::from_secs(300);

/// Default timeout for waiting on a specific L2 transaction to be *mined*
/// (included in a sealed block, before finalization).
pub const TX_MINED_TIMEOUT: Duration = Duration::from_secs(60);

/// A ZKsync OS chain handle. Obtain one via `Ecosystem::chain()` /
/// `Ecosystem::chains()`, or from a specialized fixture (e.g. the v30 upgrade
/// fixture).
///
/// Carries the chain's identity plus its pre-funded test wallets. All write
/// operations default to `wallet(0)` (the ZKsync rich account). Some fixtures
/// drive a chain entirely through L1 admin keys and provide no test wallets —
/// such a chain has an empty wallet set and the wallet-backed operations
/// (`transfer`, `send_tx`, `ping`, …) are not available on it.
pub struct Chain {
    pub(crate) chain_id: u64,
    pub(crate) bridgehub_addr: Address,
    pub(crate) l1_rpc: String,
    pub(crate) l2_rpc: String,
    pub(crate) wallets: Vec<PrivateKeySigner>,
    /// This chain's position in its ecosystem. Selects the chain's two dedicated
    /// activity wallets from `activity::ACTIVITY_WALLET_KEYS` (transfer wallet at
    /// `2*pos`, deposit wallet at `2*pos+1`). Set by `Ecosystem::assemble`; 0 for
    /// chains built outside an ecosystem.
    pub(crate) activity_chain_index: usize,
    /// The background activity currently running on this chain, if any. `Some`
    /// while activity runs; `finish_activity`/`stop_activity` take it out (and
    /// return the verdict), and chain teardown drops it (aborting the loops).
    /// Acts as the single-run guard: starting while it is `Some` panics.
    pub(crate) running_activity: Mutex<Option<ActivityHandle>>,
}

impl Chain {
    /// Construct a chain handle. `wallets` may be empty for chains driven only
    /// through L1 admin keys (e.g. the v30 upgrade fixture).
    pub(crate) fn new(
        chain_id: u64,
        bridgehub_addr: Address,
        l1_rpc: String,
        l2_rpc: String,
        wallets: Vec<PrivateKeySigner>,
        activity_chain_index: usize,
    ) -> Self {
        Self {
            chain_id,
            bridgehub_addr,
            l1_rpc,
            l2_rpc,
            wallets,
            activity_chain_index,
            running_activity: Mutex::new(None),
        }
    }

    // ── Identity ─────────────────────────────────────────────────────────────

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn bridgehub_addr(&self) -> Address {
        self.bridgehub_addr
    }

    pub fn l1_rpc_url(&self) -> &str {
        &self.l1_rpc
    }

    pub fn l2_rpc_url(&self) -> &str {
        &self.l2_rpc
    }

    pub fn wallets(&self) -> &[PrivateKeySigner] {
        &self.wallets
    }

    pub fn wallet(&self, i: usize) -> &PrivateKeySigner {
        self.wallets.get(i).unwrap_or_else(|| {
            panic!("chain has no wallet #{i} — this chain was created without funded test wallets")
        })
    }

    pub async fn l2_provider(&self) -> Result<impl Provider> {
        ProviderBuilder::new()
            .connect(&self.l2_rpc)
            .await
            .context("connect to L2")
    }

    pub async fn l1_provider(&self) -> Result<impl Provider> {
        ProviderBuilder::new()
            .connect(&self.l1_rpc)
            .await
            .context("connect to L1")
    }

    /// ETH transfer from `wallet(0)` to `to`.
    pub async fn transfer(&self, to: Address, amount: U256) -> Result<TxHash> {
        self.transfer_from(&self.wallet(0).clone(), to, amount)
            .await
    }

    /// ETH transfer from an explicit wallet to `to`.
    pub async fn transfer_from(
        &self,
        wallet: &PrivateKeySigner,
        to: Address,
        amount: U256,
    ) -> Result<TxHash> {
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet.clone()))
            .connect(&self.l2_rpc)
            .await
            .context("connect to L2 with wallet")?;
        let tx = TransactionRequest::default().with_to(to).with_value(amount);
        Ok(*provider
            .send_transaction(tx)
            .await
            .context("send transfer")?
            .tx_hash())
    }

    /// Arbitrary L2 transaction signed by `wallet(0)`.
    pub async fn send_tx(&self, tx: TransactionRequest) -> Result<TxHash> {
        self.send_tx_from(&self.wallet(0).clone(), tx).await
    }

    /// Arbitrary L2 transaction signed by an explicit wallet.
    pub async fn send_tx_from(
        &self,
        wallet: &PrivateKeySigner,
        tx: TransactionRequest,
    ) -> Result<TxHash> {
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(wallet.clone()))
            .connect(&self.l2_rpc)
            .await
            .context("connect to L2 with wallet")?;
        Ok(*provider
            .send_transaction(tx)
            .await
            .context("send tx")?
            .tx_hash())
    }

    /// 1 wei self-transfer from `wallet(0)` — gives the sequencer work to seal a batch.
    pub async fn ping(&self) -> Result<TxHash> {
        self_transfer(&self.l2_rpc, &self.wallet(0).clone()).await
    }

    /// Start background activity ("noise") on this chain per `config`.
    ///
    /// Spawns one tokio task per enabled flow; the chain owns the run. Control it
    /// with [`pause_activity`](Self::pause_activity) /
    /// [`resume_activity`](Self::resume_activity) and end it with a
    /// finalized-or-fail verdict via [`finish_activity`](Self::finish_activity)
    /// (waits for the configured targets) or [`stop_activity`](Self::stop_activity)
    /// (ends early). Each chain uses its own two activity wallets (transfer +
    /// deposit), so flows never race on nonces.
    ///
    /// Panics if activity is already running on this chain — two loops would race
    /// on the same activity wallets. End the existing run first.
    pub fn start_activity(&self, config: ActivityConfig) {
        let mut slot = self.running_activity.lock().unwrap();
        if slot.is_some() {
            panic!(
                "background activity already running on chain {} — \
                 finish_activity/stop_activity before starting again",
                self.chain_id
            );
        }

        let state = ActivityState::new();
        let mut tasks = Vec::new();

        if let Some(flow) = config.l2_transfers {
            let key = ACTIVITY_WALLET_KEYS[transfer_wallet_index(self.activity_chain_index)];
            let signer: PrivateKeySigner = key.parse().expect("parse transfer wallet key");
            tasks.push(tokio::spawn(run_l2_transfers(
                self.l2_rpc.clone(),
                signer,
                flow,
                state.clone(),
            )));
        }

        let deposit_recipient = config.l1_deposits.map(|flow| {
            let depositor_sk =
                ACTIVITY_WALLET_KEYS[deposit_wallet_index(self.activity_chain_index)].to_string();
            let recipient = depositor_sk
                .parse::<PrivateKeySigner>()
                .expect("parse deposit wallet key")
                .address();
            tasks.push(tokio::spawn(run_l1_deposits(
                self.l1_rpc.clone(),
                self.l2_rpc.clone(),
                self.bridgehub_addr,
                self.chain_id,
                depositor_sk,
                recipient,
                flow,
                state.clone(),
            )));
            recipient
        });

        *slot = Some(ActivityHandle::new(
            state,
            tasks,
            self.l2_rpc.clone(),
            deposit_recipient,
        ));
    }

    /// Pause submissions from the running activity. Panics if none is running.
    pub fn pause_activity(&self) {
        self.running_activity
            .lock()
            .unwrap()
            .as_ref()
            .expect("no background activity running on this chain")
            .pause();
    }

    /// Resume after [`pause_activity`](Self::pause_activity). Panics if none is
    /// running.
    pub fn resume_activity(&self) {
        self.running_activity
            .lock()
            .unwrap()
            .as_ref()
            .expect("no background activity running on this chain")
            .resume();
    }

    /// Wait for every flow to reach its target, then verify finalization and
    /// return the verdict. For bounded configs (`Count`/`Duration`); for an
    /// `Unbounded` flow use [`stop_activity`](Self::stop_activity).
    ///
    /// `Err` if any submitted transaction failed to finalize (revert, drop,
    /// timeout) or a loop died. Errors if no activity is running.
    pub async fn finish_activity(&self) -> Result<ActivityReport> {
        // Take the handle out before awaiting — a std Mutex guard cannot be held
        // across .await, and removing it here also clears the single-run guard.
        let handle = self
            .running_activity
            .lock()
            .unwrap()
            .take()
            .context("no background activity running on this chain")?;
        handle.await_done().await
    }

    /// Stop all flows now, then verify finalization of everything submitted so
    /// far and return the verdict. Use for `Unbounded` configs or to end early.
    /// Errors if no activity is running.
    pub async fn stop_activity(&self) -> Result<ActivityReport> {
        let handle = self
            .running_activity
            .lock()
            .unwrap()
            .take()
            .context("no background activity running on this chain")?;
        handle.stop().await
    }

    /// L2 self-transfers submitted by the running activity so far (0 if none).
    pub(crate) fn activity_transfers_submitted(&self) -> u64 {
        self.running_activity
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |h| h.transfers_submitted())
    }

    /// L1→L2 deposits submitted by the running activity so far (0 if none).
    pub(crate) fn activity_deposits_submitted(&self) -> u64 {
        self.running_activity
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |h| h.deposits_submitted())
    }

    /// True if the running activity has given up after repeated errors.
    pub(crate) fn activity_failed(&self) -> bool {
        self.running_activity
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.failed())
    }

    /// L2 ETH balance of `addr`.
    pub async fn balance(&self, addr: Address) -> Result<U256> {
        ProviderBuilder::new()
            .connect(&self.l2_rpc)
            .await
            .context("connect to L2")?
            .get_balance(addr)
            .await
            .context("eth_getBalance")
    }

    /// Current latest (produced) L2 block number.
    pub async fn latest_block(&self) -> anyhow::Result<u64> {
        let block = self
            .l2_provider()
            .await?
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .context("eth_getBlockByNumber(latest)")?
            .context("latest block not found")?;
        Ok(block.header.number)
    }

    /// Current finalized L2 block number (committed, proved and executed on L1).
    /// Returns 0 if no batch has been committed yet (server returns null for "finalized").
    pub async fn finalized_block(&self) -> anyhow::Result<u64> {
        let block = self
            .l2_provider()
            .await?
            .get_block_by_number(BlockNumberOrTag::Finalized)
            .await
            .context("eth_getBlockByNumber(finalized)")?;
        Ok(block.map(|b| b.header.number).unwrap_or(0))
    }

    /// Wait until the latest (produced) L2 block reaches `block`.
    /// Times out after [`BLOCK_PRODUCED_TIMEOUT`].
    pub async fn wait_for_block_produced(&self, block: u64) -> anyhow::Result<()> {
        lib_server::wait_for_l2_block_produced(&self.l2_rpc, block, BLOCK_PRODUCED_TIMEOUT).await
    }

    /// Wait until the finalized L2 block reaches `block`.
    /// Times out after [`BLOCK_FINALIZED_TIMEOUT`].
    pub async fn wait_for_block_finalized(&self, block: u64) -> anyhow::Result<()> {
        lib_server::wait_for_l2_block_finalized(&self.l2_rpc, block, BLOCK_FINALIZED_TIMEOUT).await
    }

    /// Wait until the L2 finalized block advances past its current value.
    /// Returns the new finalized block number.
    pub async fn wait_for_batch(&self) -> Result<u64> {
        let current = self.finalized_block().await?;
        self.wait_for_block_finalized(current + 1)
            .await
            .context("wait_for_batch timed out")?;
        self.finalized_block()
            .await
            .context("get_l2_finalized_block after batch")
    }

    /// Wait until the block containing `hash` is finalized (committed,
    /// proved and executed on L1). Returns the block number.
    ///
    /// Prefer this over [`Self::wait_for_batch`] whenever a specific
    /// transaction is the thing being settled: `wait_for_batch` waits for
    /// "one more batch than is finalized *now*", which deadlocks if the tx's
    /// batch already finalized in the meantime (e.g. while waiting on
    /// another chain in a multi-chain test) and no further traffic arrives.
    pub async fn wait_for_tx_finalized(&self, hash: TxHash) -> Result<u64> {
        let receipt = self.wait_for_tx(hash).await?;
        let block = receipt.block_number.context("mined tx has block number")?;
        self.wait_for_block_finalized(block)
            .await
            .context("wait_for_tx_finalized timed out")?;
        Ok(block)
    }

    /// Wait until a specific L2 transaction is mined (polls every 500 ms,
    /// [`TX_MINED_TIMEOUT`]).
    pub async fn wait_for_tx(&self, hash: TxHash) -> Result<TransactionReceipt> {
        let provider = ProviderBuilder::new()
            .connect(&self.l2_rpc)
            .await
            .context("connect to L2")?;
        let start = std::time::Instant::now();
        let deadline = start + TX_MINED_TIMEOUT;
        let mut last_log = start;
        loop {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("tx {hash:#x} not mined within {TX_MINED_TIMEOUT:?}");
            }
            match provider.get_transaction_receipt(hash).await {
                Ok(Some(receipt)) => {
                    anyhow::ensure!(receipt.status(), "tx {hash:#x} reverted on L2");
                    return Ok(receipt);
                }
                Ok(None) => {}
                Err(e) => {
                    if last_log.elapsed() >= Duration::from_secs(10) {
                        eprintln!(
                            "[wait_for_tx] RPC error: {e} (elapsed={:.0}s)",
                            start.elapsed().as_secs_f64()
                        );
                        last_log = std::time::Instant::now();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
