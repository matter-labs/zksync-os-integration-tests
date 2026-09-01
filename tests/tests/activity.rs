use std::time::Duration;

use anyhow::Result;
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::{ActivityConfig, Ecosystem, FlowConfig};

/// Feature demonstration: on a fresh ecosystem, run several short-lived activity
/// configurations one after another and confirm each reaches a passing verdict —
/// `await_done()` returns `Ok` only when every submitted transaction finalized
/// on L1. The double-start guard clears after each verdict, so the runs are
/// sequential on the same chain.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn short_activities_all_succeed(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let chain = eco.chain();

    // 1. A fixed count of L2 transfers.
    chain.start_activity(ActivityConfig {
        l2_transfers: Some(FlowConfig::count(Duration::from_millis(300), 5)),
        l1_deposits: None,
    });
    let report = chain.finish_activity().await?;
    assert_eq!(report.transfers_submitted, 5);
    assert_eq!(report.transfers_finalized, 5);

    // 2. A fixed count of L1→L2 deposits.
    chain.start_activity(ActivityConfig {
        l2_transfers: None,
        l1_deposits: Some(FlowConfig::count(Duration::from_secs(1), 2)),
    });
    let report = chain.finish_activity().await?;
    assert_eq!(report.deposits_submitted, 2);

    // 3. Both flows together, time-bounded.
    chain.start_activity(ActivityConfig {
        l2_transfers: Some(FlowConfig::for_duration(
            Duration::from_millis(300),
            Duration::from_secs(2),
        )),
        l1_deposits: Some(FlowConfig::for_duration(
            Duration::from_secs(1),
            Duration::from_secs(2),
        )),
    });
    let report = chain.finish_activity().await?;
    assert!(report.transfers_submitted >= 1, "expected some transfers");
    assert_eq!(report.transfers_finalized, report.transfers_submitted);
    assert!(report.deposits_submitted >= 1, "expected some deposits");

    Ok(())
}
