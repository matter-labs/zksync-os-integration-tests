//! Opt-in background activity ("noise") for integration tests.
//!
//! When enabled, dedicated activity wallets fire L2 self-transfers and/or L1→L2
//! deposits on a chain while a test runs, so the test exercises a live, moving
//! chain rather than an idle one. Each chain owns two activity wallets — one for
//! transfers, one for deposits — so neither flow shares a nonce sequence with
//! anything else. See `Chain::start_activity`.
//!
//! Activity has a target (a fixed count, a fixed duration, or unbounded) and a
//! single pass/fail verdict: on completion every submitted transaction must have
//! reached L1 finalization. See [`ActivityHandle::await_done`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::eips::BlockId;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{bail, Context as _, Result};
use tokio::task::JoinHandle;
use zk_deployer::l1_l2_deposit::{deposit_eth, DEFAULT_L1_TO_L2_GAS_PRICE};

use crate::chain::{BLOCK_FINALIZED_TIMEOUT, TX_MINED_TIMEOUT};

/// After this many consecutive failed iterations with no success in between, a
/// flow task gives up: it sets the `failed` flag and exits. The final verdict
/// then reports failure rather than hanging on a broken loop.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Back-off slept after a failed iteration before the next retry.
pub const CRASH_RESTART_DELAY: Duration = Duration::from_secs(3);

/// Amount of ETH per background L1→L2 deposit (1 ETH). The deposit wallet is
/// pre-funded with 10,000 ETH on L1 (Anvil `--accounts`), so it lasts the whole
/// test run.
pub(crate) const ACTIVITY_DEPOSIT_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000;

/// L2 ETH funded to each chain's *transfer* wallet at setup (via an L1→L2
/// deposit) so it can pay gas for background self-transfers. The deposit wallet
/// needs no L2 funding — it only ever receives deposits, never sends L2 txs.
pub(crate) const ACTIVITY_WALLET_L2_FUND_ETH: u64 = 10;

/// Private activity-wallet pool: Anvil HD mnemonic accounts #10–#19.
///
/// Kept private to this module (and out of `zk-deployer`, whose `l1_l2_deposit`
/// is a public module) so test code cannot reach these keys — the
/// test-wallet / activity-wallet boundary is enforced by visibility.
///
/// Each chain claims two consecutive entries: a transfer wallet and a deposit
/// wallet (see [`transfer_wallet_index`] / [`deposit_wallet_index`]). With ten
/// keys that supports up to [`max_activity_chains`] chains.
///
/// `default_builder()` passes `--accounts 20`, so these accounts are pre-funded
/// with ETH on L1.
pub(crate) const ACTIVITY_WALLET_KEYS: [&str; 10] = [
    // Anvil #10 — 0xBcd4042DE499D14e55001CcbB24a551F3b954096
    "0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897",
    // Anvil #11 — 0x71bE63f3384f5fb98995898A86B02Fb2426c5788
    "0x701b615bbdfb9de65240bc28bd21bbc0d996645a3dd57e7b12bc2bdf6f192c82",
    // Anvil #12 — 0xFABB0ac9d68B0B445fB7357272Ff202C5651694a
    "0xa267530f49f8280200edf313ee7af6b827f2a8bce2897751d06a843f644967b1",
    // Anvil #13 — 0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec
    "0x47c99abed3324a2707c28affff1267e45918ec8c3f20b8aa892e8b065d2942dd",
    // Anvil #14 — 0xdF3e18d64BC6A983f673Ab319CCaE4f1a57C7097
    "0xc526ee95bf44d8fc405a158bb884d9d1238d99f0612e9f33d006bb0789009aaa",
    // Anvil #15 — 0xcd3B766CCDd6AE721141F452C550Ca635964ce71
    "0x8166f546bab6da521a8369cab06c5d2b9e46670292d85c875ee9ec20e84ffb61",
    // Anvil #16 — 0x2546BcD3c84621e976D8185a91A922aE77ECEc30
    "0xea6c44ac03bff858b476bba40716402b03e41b8e97e276d1baec7c37d42484a0",
    // Anvil #17 — 0xbDA5747bFD65F08deb54cb465eB87D40e51B197E
    "0x689af8efa8c651a91ad287602527f3af2fe9f6501a7ac4b061667b5a93e037fd",
    // Anvil #18 — 0xdD2FD4581271e230360230F9337D5c0430Bf44C0
    "0xde9be858da4a475276426320d5e9262ecfc3ba460bfac56360bfa6c4c28b4ee0",
    // Anvil #19 — 0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199
    "0xdf57089febbacf7ba0bc227dafbffa9fc08a93fdc68e1e42411a14efcf23656e",
];

/// Pool index of chain `chain_pos`'s transfer wallet.
pub(crate) fn transfer_wallet_index(chain_pos: usize) -> usize {
    2 * chain_pos
}

/// Pool index of chain `chain_pos`'s deposit wallet.
pub(crate) fn deposit_wallet_index(chain_pos: usize) -> usize {
    2 * chain_pos + 1
}

/// How many chains the wallet pool of length `pool_len` can serve — two wallets
/// (transfer + deposit) per chain.
pub(crate) fn max_activity_chains(pool_len: usize) -> usize {
    pool_len / 2
}

/// When a flow stops submitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Submit exactly this many transactions, then stop.
    Count(u64),
    /// Submit at the flow's interval for this long, then stop.
    Duration(Duration),
    /// Run until the handle is stopped.
    Unbounded,
}

/// One activity flow: how often to submit, and when to stop.
#[derive(Clone, Copy, Debug)]
pub struct FlowConfig {
    pub interval: Duration,
    pub target: Target,
}

impl FlowConfig {
    pub fn new(interval: Duration, target: Target) -> Self {
        Self { interval, target }
    }

    /// `count` submissions at `interval`, then stop.
    pub fn count(interval: Duration, count: u64) -> Self {
        Self::new(interval, Target::Count(count))
    }

    /// Submit at `interval` for `duration`, then stop.
    pub fn for_duration(interval: Duration, duration: Duration) -> Self {
        Self::new(interval, Target::Duration(duration))
    }

    /// Submit at `interval` until the handle is stopped.
    pub fn unbounded(interval: Duration) -> Self {
        Self::new(interval, Target::Unbounded)
    }
}

/// Default interval between L2 self-transfers.
pub const DEFAULT_TRANSFER_INTERVAL: Duration = Duration::from_millis(500);
/// Default interval between L1→L2 deposits.
pub const DEFAULT_DEPOSIT_INTERVAL: Duration = Duration::from_secs(5);

/// What background activity to run on a chain. `None` disables a flow, so this
/// expresses "transfers only", "deposits only", or "both".
#[derive(Clone)]
pub struct ActivityConfig {
    pub l2_transfers: Option<FlowConfig>,
    pub l1_deposits: Option<FlowConfig>,
}

impl Default for ActivityConfig {
    /// Both flows, unbounded, at the default intervals.
    fn default() -> Self {
        Self {
            l2_transfers: Some(FlowConfig::unbounded(DEFAULT_TRANSFER_INTERVAL)),
            l1_deposits: Some(FlowConfig::unbounded(DEFAULT_DEPOSIT_INTERVAL)),
        }
    }
}

impl ActivityConfig {
    /// Unbounded L2 transfers only, no deposits.
    pub fn transfers_only() -> Self {
        Self {
            l2_transfers: Some(FlowConfig::unbounded(DEFAULT_TRANSFER_INTERVAL)),
            l1_deposits: None,
        }
    }

    /// Unbounded L1→L2 deposits only, no transfers.
    pub fn deposits_only() -> Self {
        Self {
            l2_transfers: None,
            l1_deposits: Some(FlowConfig::unbounded(DEFAULT_DEPOSIT_INTERVAL)),
        }
    }
}

/// Outcome of a completed activity run. Returned by
/// [`ActivityHandle::await_done`] / [`ActivityHandle::stop`] only when the
/// verdict passed — every submitted transaction reached finalization.
#[derive(Debug, Clone)]
pub struct ActivityReport {
    /// L2 self-transfers submitted (tx hash accepted by the RPC).
    pub transfers_submitted: u64,
    /// Of those, how many were confirmed finalized with a success receipt.
    pub transfers_finalized: u64,
    /// L1→L2 deposits submitted (L1 tx confirmed).
    pub deposits_submitted: u64,
    /// Observed increase in the deposit wallet's finalized L2 balance. At least
    /// `deposits_submitted * ACTIVITY_DEPOSIT_AMOUNT_WEI` — the recipient also
    /// receives each deposit's gas refund, so this runs slightly higher.
    pub deposit_balance_delta: U256,
}

struct ActivityStateInner {
    transfers_submitted: AtomicU64,
    deposits_submitted: AtomicU64,
    transfer_hashes: Mutex<Vec<TxHash>>,
    /// Deposit recipient's finalized L2 balance captured once, before the first
    /// deposit. The verdict measures the delta against this baseline rather than
    /// assuming zero — a chain reused across runs starts non-empty.
    deposit_baseline: Mutex<Option<U256>>,
    paused: AtomicBool,
    stopped: AtomicBool,
    failed: AtomicBool,
}

/// Shared, cloneable state between the handle and its background tasks. The
/// tasks are the writers; the handle reads and controls (pause/stop).
#[derive(Clone)]
pub(crate) struct ActivityState(Arc<ActivityStateInner>);

impl ActivityState {
    pub(crate) fn new() -> Self {
        Self(Arc::new(ActivityStateInner {
            transfers_submitted: AtomicU64::new(0),
            deposits_submitted: AtomicU64::new(0),
            transfer_hashes: Mutex::new(Vec::new()),
            deposit_baseline: Mutex::new(None),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }))
    }

    fn record_transfer(&self, hash: TxHash) {
        self.0.transfer_hashes.lock().unwrap().push(hash);
        self.0.transfers_submitted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_deposit(&self) {
        self.0.deposits_submitted.fetch_add(1, Ordering::Relaxed);
    }

    fn transfers_submitted(&self) -> u64 {
        self.0.transfers_submitted.load(Ordering::Relaxed)
    }

    fn deposits_submitted(&self) -> u64 {
        self.0.deposits_submitted.load(Ordering::Relaxed)
    }

    fn transfer_hashes(&self) -> Vec<TxHash> {
        self.0.transfer_hashes.lock().unwrap().clone()
    }

    fn set_deposit_baseline(&self, balance: U256) {
        *self.0.deposit_baseline.lock().unwrap() = Some(balance);
    }

    fn deposit_baseline(&self) -> Option<U256> {
        *self.0.deposit_baseline.lock().unwrap()
    }

    fn mark_failed(&self) {
        self.0.failed.store(true, Ordering::Relaxed);
    }

    fn is_failed(&self) -> bool {
        self.0.failed.load(Ordering::Relaxed)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.0.paused.store(paused, Ordering::Relaxed);
    }

    fn is_paused(&self) -> bool {
        self.0.paused.load(Ordering::Relaxed)
    }

    fn set_stopped(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
    }

    fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Relaxed)
    }
}

/// The running background activity a [`Chain`](crate::chain::Chain) owns. Not
/// constructed by test code — drive it through `Chain::start_activity` /
/// `pause_activity` / `resume_activity` / `finish_activity` / `stop_activity`.
/// Dropping it (when the chain drops) aborts the loops without a verdict.
pub(crate) struct ActivityHandle {
    state: ActivityState,
    tasks: Vec<JoinHandle<()>>,
    l2_rpc: String,
    /// `Some` when the deposit flow is enabled — the wallet whose finalized L2
    /// balance the verdict checks.
    deposit_recipient: Option<Address>,
}

impl ActivityHandle {
    pub(crate) fn new(
        state: ActivityState,
        tasks: Vec<JoinHandle<()>>,
        l2_rpc: String,
        deposit_recipient: Option<Address>,
    ) -> Self {
        Self {
            state,
            tasks,
            l2_rpc,
            deposit_recipient,
        }
    }

    /// Pause new submissions. An in-flight submission may still complete.
    pub(crate) fn pause(&self) {
        self.state.set_paused(true);
    }

    /// Resume after [`pause`](Self::pause).
    pub(crate) fn resume(&self) {
        self.state.set_paused(false);
    }

    /// L2 self-transfers submitted so far — used by the fixture warm-up.
    pub(crate) fn transfers_submitted(&self) -> u64 {
        self.state.transfers_submitted()
    }

    /// L1→L2 deposits submitted so far — used by the fixture warm-up.
    pub(crate) fn deposits_submitted(&self) -> u64 {
        self.state.deposits_submitted()
    }

    /// True once a flow has given up after repeated errors. Lets a fixture-owned
    /// run detect a silently-dead loop without taking the verdict.
    pub(crate) fn failed(&self) -> bool {
        self.state.is_failed()
    }

    /// Wait for every flow to reach its target, then verify finalization.
    ///
    /// Only resolves for bounded configs (`Count`/`Duration`). If any flow is
    /// `Unbounded`, use [`stop`](Self::stop) instead — `await_done` would wait
    /// for a target that never arrives.
    pub(crate) async fn await_done(self) -> Result<ActivityReport> {
        self.finish(false).await
    }

    /// Stop all flows now, then verify finalization of everything submitted so
    /// far. Use this for `Unbounded` configs or to end a run early.
    pub(crate) async fn stop(self) -> Result<ActivityReport> {
        self.finish(true).await
    }

    async fn finish(mut self, stop_now: bool) -> Result<ActivityReport> {
        if stop_now {
            self.state.set_stopped();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }

        if self.state.is_failed() {
            bail!("background activity loop gave up after repeated errors");
        }

        let transfers_submitted = self.state.transfers_submitted();
        let transfers_finalized =
            verify_transfers_finalized(&self.l2_rpc, self.state.transfer_hashes()).await?;

        let deposits_submitted = self.state.deposits_submitted();
        let deposit_balance_delta = match self.deposit_recipient {
            Some(recipient) => {
                let baseline = self.state.deposit_baseline().context(
                    "deposit flow never captured a balance baseline (loop died at startup)",
                )?;
                verify_deposits_finalized(
                    &self.l2_rpc,
                    recipient,
                    baseline,
                    deposits_submitted,
                    ACTIVITY_DEPOSIT_AMOUNT_WEI,
                )
                .await?
            }
            None => U256::ZERO,
        };

        Ok(ActivityReport {
            transfers_submitted,
            transfers_finalized,
            deposits_submitted,
            deposit_balance_delta,
        })
    }
}

impl Drop for ActivityHandle {
    fn drop(&mut self) {
        // Teardown only: abort without awaiting and without a verdict. Callers
        // that want a verdict must call await_done()/stop(). The owning Chain's
        // `running_activity` slot is what tracks "is activity running"; dropping
        // the handle (chain teardown, or after finish takes it out) just stops
        // the loops.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// True once this flow's `target` has been met given `submitted` count and the
/// time `elapsed` since the flow started.
fn target_reached(target: Target, submitted: u64, elapsed: Duration) -> bool {
    match target {
        Target::Count(n) => submitted >= n,
        Target::Duration(d) => elapsed >= d,
        Target::Unbounded => false,
    }
}

/// Sleep until `deadline`, or return immediately if it has already passed.
async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        tokio::time::sleep(deadline - now).await;
    }
}

/// One 1-wei self-transfer. Submitted to the mempool; the accepted hash is
/// returned without watching it mine. Shared by background activity and
/// `Chain::ping`, which are the same primitive from different wallets.
pub(crate) async fn self_transfer(l2_rpc: &str, signer: &PrivateKeySigner) -> Result<TxHash> {
    let addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect(l2_rpc)
        .await?;
    let tx = TransactionRequest::default()
        .with_to(addr)
        .with_value(U256::from(1u64));
    Ok(*provider.send_transaction(tx).await?.tx_hash())
}

/// L2 self-transfer loop. One dedicated `signer`, so its nonce sequence is never
/// shared — no mutex needed.
pub(crate) async fn run_l2_transfers(
    l2_rpc: String,
    signer: PrivateKeySigner,
    flow: FlowConfig,
    state: ActivityState,
) {
    let start = Instant::now();
    let mut consecutive_errors = 0u32;
    loop {
        if state.is_stopped() || target_reached(flow.target, state.transfers_submitted(), start.elapsed())
        {
            return;
        }
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            state.mark_failed();
            eprintln!("[activity] l2_transfers gave up after {MAX_CONSECUTIVE_ERRORS} errors");
            return;
        }
        if state.is_paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let tick_start = Instant::now();
        match self_transfer(&l2_rpc, &signer).await {
            Ok(hash) => {
                state.record_transfer(hash);
                consecutive_errors = 0;
                sleep_until(tick_start + flow.interval).await;
            }
            Err(e) => {
                eprintln!("[activity] l2 transfer error: {e:#}");
                consecutive_errors += 1;
                tokio::time::sleep(flow.interval.min(CRASH_RESTART_DELAY)).await;
            }
        }
    }
}

/// L1→L2 deposit loop. One dedicated `depositor_sk` for this chain, so its L1
/// nonce sequence is never shared with another chain's loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_l1_deposits(
    l1_rpc: String,
    l2_rpc: String,
    bridgehub: Address,
    chain_id: u64,
    depositor_sk: String,
    recipient: Address,
    flow: FlowConfig,
    state: ActivityState,
) {
    // Capture the recipient's finalized L2 balance before any deposit, so the
    // verdict measures the delta this run produced — not absolute balance.
    match capture_finalized_balance(&l2_rpc, recipient).await {
        Ok(baseline) => state.set_deposit_baseline(baseline),
        Err(e) => {
            eprintln!("[activity] l1_deposits could not read baseline balance: {e:#}");
            state.mark_failed();
            return;
        }
    }

    let amount = U256::from(ACTIVITY_DEPOSIT_AMOUNT_WEI);
    let start = Instant::now();
    let mut consecutive_errors = 0u32;
    loop {
        if state.is_stopped() || target_reached(flow.target, state.deposits_submitted(), start.elapsed())
        {
            return;
        }
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            state.mark_failed();
            eprintln!("[activity] l1_deposits gave up after {MAX_CONSECUTIVE_ERRORS} errors");
            return;
        }
        if state.is_paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let tick_start = Instant::now();
        match deposit_eth(
            &l1_rpc,
            bridgehub,
            chain_id,
            recipient,
            amount,
            DEFAULT_L1_TO_L2_GAS_PRICE,
            &depositor_sk,
        )
        .await
        {
            Ok(_) => {
                state.record_deposit();
                consecutive_errors = 0;
                sleep_until(tick_start + flow.interval).await;
            }
            Err(e) => {
                eprintln!("[activity] l1 deposit error: {e:#}");
                consecutive_errors += 1;
                tokio::time::sleep(flow.interval.min(CRASH_RESTART_DELAY)).await;
            }
        }
    }
}

/// Wait until every submitted transfer hash is mined with a success receipt and
/// the block containing it is finalized. Returns the finalized count (== input
/// length on success); errors on any revert, drop, or finalization timeout.
async fn verify_transfers_finalized(l2_rpc: &str, hashes: Vec<TxHash>) -> Result<u64> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2 for transfer verification")?;

    let mut max_block = 0u64;
    for hash in &hashes {
        let deadline = Instant::now() + TX_MINED_TIMEOUT;
        let receipt = loop {
            if let Some(r) = provider
                .get_transaction_receipt(*hash)
                .await
                .context("eth_getTransactionReceipt")?
            {
                break r;
            }
            if Instant::now() >= deadline {
                bail!("transfer {hash} was not mined within {TX_MINED_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        if !receipt.status() {
            bail!("transfer {hash} reverted");
        }
        max_block = max_block.max(receipt.block_number.unwrap_or(0));
    }

    lib_server::wait_for_l2_block_finalized(l2_rpc, max_block, BLOCK_FINALIZED_TIMEOUT)
        .await
        .with_context(|| format!("transfers not finalized up to block {max_block}"))?;

    Ok(hashes.len() as u64)
}

/// Read `addr`'s balance at the `finalized` block tag.
async fn capture_finalized_balance(l2_rpc: &str, addr: Address) -> Result<U256> {
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2")?;
    provider
        .get_balance(addr)
        .block_id(BlockId::finalized())
        .await
        .context("eth_getBalance(finalized)")
}

/// Wait until the deposit wallet's *finalized* L2 balance has risen by at least
/// `count * amount` over `baseline`. Returns the observed delta; errors if it
/// never reaches that threshold within the finalization window.
///
/// The bound is `>=`, not `==`: each deposit sets `refundRecipient = recipient`,
/// so the recipient also receives the unused-gas refund on top of `l2Value`. A
/// refund is far smaller than the 1-ETH deposit, so reaching `count * amount`
/// can only happen once all `count` deposits have credited their `l2Value`.
async fn verify_deposits_finalized(
    l2_rpc: &str,
    recipient: Address,
    baseline: U256,
    count: u64,
    amount: u128,
) -> Result<U256> {
    let expected = U256::from(amount) * U256::from(count);
    let deadline = Instant::now() + BLOCK_FINALIZED_TIMEOUT;
    loop {
        let balance = capture_finalized_balance(l2_rpc, recipient).await?;
        let delta = balance.saturating_sub(baseline);
        if delta >= expected {
            return Ok(delta);
        }
        if Instant::now() >= deadline {
            bail!(
                "deposits not finalized: expected balance delta >= {expected}, observed {delta} \
                 after {BLOCK_FINALIZED_TIMEOUT:?} ({count} deposits submitted)"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_wallet_index_is_even() {
        assert_eq!(transfer_wallet_index(0), 0);
        assert_eq!(transfer_wallet_index(1), 2);
        assert_eq!(transfer_wallet_index(2), 4);
    }

    #[test]
    fn deposit_wallet_index_is_odd() {
        assert_eq!(deposit_wallet_index(0), 1);
        assert_eq!(deposit_wallet_index(1), 3);
        assert_eq!(deposit_wallet_index(2), 5);
    }

    #[test]
    fn each_chain_claims_two_distinct_wallets() {
        for i in 0..max_activity_chains(ACTIVITY_WALLET_KEYS.len()) {
            assert_ne!(transfer_wallet_index(i), deposit_wallet_index(i));
            assert!(deposit_wallet_index(i) < ACTIVITY_WALLET_KEYS.len());
        }
    }

    #[test]
    fn max_activity_chains_is_half_the_pool() {
        assert_eq!(max_activity_chains(10), 5);
        assert_eq!(max_activity_chains(2), 1);
        assert_eq!(max_activity_chains(3), 1);
    }

    #[test]
    fn default_enables_both_unbounded() {
        let c = ActivityConfig::default();
        assert_eq!(c.l2_transfers.map(|f| f.target), Some(Target::Unbounded));
        assert_eq!(c.l1_deposits.map(|f| f.target), Some(Target::Unbounded));
    }

    #[test]
    fn transfers_only_disables_deposits() {
        let c = ActivityConfig::transfers_only();
        assert!(c.l2_transfers.is_some());
        assert!(c.l1_deposits.is_none());
    }

    #[test]
    fn deposits_only_disables_transfers() {
        let c = ActivityConfig::deposits_only();
        assert!(c.l2_transfers.is_none());
        assert!(c.l1_deposits.is_some());
    }

    #[test]
    fn key_pool_has_ten_entries() {
        assert_eq!(ACTIVITY_WALLET_KEYS.len(), 10);
    }

    #[test]
    fn target_reached_semantics() {
        assert!(target_reached(Target::Count(3), 3, Duration::ZERO));
        assert!(!target_reached(Target::Count(3), 2, Duration::ZERO));
        assert!(target_reached(
            Target::Duration(Duration::from_secs(1)),
            0,
            Duration::from_secs(2)
        ));
        assert!(!target_reached(
            Target::Duration(Duration::from_secs(5)),
            0,
            Duration::from_secs(2)
        ));
        assert!(!target_reached(Target::Unbounded, 1_000, Duration::from_secs(60)));
    }
}
