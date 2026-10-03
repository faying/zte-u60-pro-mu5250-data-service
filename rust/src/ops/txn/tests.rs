//! 状态机用例矩阵（write-op-layer.md 测试计划 Critical Paths 第 1 条，纯逻辑部分）。

use super::*;

const D: u64 = 120_000;

fn sim() -> SimId {
    SimId {
        iccid: "89860000000000000001".into(),
        slot: 1,
    }
}

fn txn(rollback: bool) -> Txn {
    let mut t = Txn::new(
        NewTxn {
            op_id: "op1".into(),
            action: "network.set_mode".into(),
            item: "network.mode".into(),
            source: Source::Screen,
            undo: false,
            target: "Only_LTE".into(),
            old: "WL_AND_5G".into(),
            rollback_to: "WL_AND_5G".into(),
            sim: sim(),
            rollback_enabled: rollback,
            deadline_ms: D,
            boot_id: "boot-a".into(),
        },
        1_000,
    );
    t.begin_apply();
    assert_eq!(t.intent, Some(Intent::Apply));
    t.applied(true, 1_000);
    t
}

fn read(v: &str, registered: bool) -> Reading {
    Reading {
        value: Some(v.into()),
        registered,
        sim: sim(),
    }
}

fn end(t: &Txn) -> (Phase, Option<Reason>) {
    (t.phase, t.reason)
}

#[test]
fn readback_and_registered_confirms() {
    let mut t = txn(false);
    assert_eq!(t.phase, Phase::Verifying);
    assert_eq!(t.on_reading(&read("WL_AND_5G", true), 3_000), Next::Wait);
    assert_eq!(t.on_reading(&read("Only_LTE", false), 5_000), Next::Wait);
    assert!(t.ever_matched);
    assert_eq!(t.on_reading(&read("Only_LTE", true), 7_000), Next::Done);
    assert_eq!(end(&t), (Phase::Confirmed, Some(Reason::Verified)));
    assert!(t.phase.is_final());
}

#[test]
fn never_matched_at_deadline_is_not_applied() {
    let mut t = txn(true);
    for now in (3_000..1_000 + D).step_by(2_000) {
        assert_eq!(t.on_reading(&read("WL_AND_5G", true), now), Next::Wait);
    }
    assert_eq!(
        t.on_reading(&read("WL_AND_5G", true), 1_000 + D),
        Next::Done
    );
    // 写没生效：不退回。
    assert_eq!(end(&t), (Phase::NotApplied, Some(Reason::Ignored)));
}

#[test]
fn apply_error_with_old_readback_is_not_applied_at_once() {
    let mut t = Txn::new(
        NewTxn {
            op_id: "op1".into(),
            action: "network.set_mode".into(),
            item: "network.mode".into(),
            source: Source::Web,
            undo: false,
            target: "Only_LTE".into(),
            old: "WL_AND_5G".into(),
            rollback_to: "WL_AND_5G".into(),
            sim: sim(),
            rollback_enabled: true,
            deadline_ms: D,
            boot_id: "boot-a".into(),
        },
        0,
    );
    t.begin_apply();
    t.applied(false, 0);
    assert_eq!(t.on_reading(&read("WL_AND_5G", true), 2_000), Next::Done);
    assert_eq!(end(&t), (Phase::NotApplied, Some(Reason::Ignored)));
}

#[test]
fn apply_error_but_readback_moved_still_confirms() {
    let mut t = txn(false);
    t.apply_failed = true;
    assert_eq!(t.on_reading(&read("Only_LTE", true), 3_000), Next::Done);
    assert_eq!(end(&t), (Phase::Confirmed, Some(Reason::Verified)));
}

#[test]
fn matched_then_old_is_manual_change() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 3_000);
    assert_eq!(t.on_reading(&read("WL_AND_5G", true), 5_000), Next::Done);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::ManualChange)));
}

#[test]
fn third_value_is_manual_change() {
    let mut t = txn(true);
    assert_eq!(t.on_reading(&read("Only_5G", true), 3_000), Next::Done);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::ManualChange)));
}

#[test]
fn empty_readback_is_not_a_third_value() {
    let mut t = txn(true);
    assert_eq!(t.on_reading(&read("", false), 3_000), Next::Wait);
    assert_eq!(t.phase, Phase::Verifying);
}

#[test]
fn deadline_with_rollback_off_is_unverified() {
    let mut t = txn(false);
    t.on_reading(&read("Only_LTE", false), 3_000);
    assert_eq!(
        t.on_reading(&read("Only_LTE", false), 1_000 + D),
        Next::Done
    );
    assert_eq!(end(&t), (Phase::Unverified, Some(Reason::NoRollback)));
}

#[test]
fn deadline_with_rollback_on_rolls_back_once() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 3_000);
    let next = t.on_reading(&read("Only_LTE", false), 1_000 + D);
    assert_eq!(next, Next::Rollback("WL_AND_5G".into()));
    assert_eq!(t.phase, Phase::RollingBack);
    assert_eq!(t.intent, Some(Intent::Rollback));
    // 退回的写还没回来：读数一律不算。
    assert_eq!(
        t.on_reading(&read("WL_AND_5G", true), 2_000 + D),
        Next::Wait
    );
    t.rollback_sent(2_000 + D);
    assert_eq!(t.intent, None);
    // 还是目标值：等。
    assert_eq!(t.on_reading(&read("Only_LTE", true), 4_000 + D), Next::Wait);
    assert_eq!(
        t.on_reading(&read("WL_AND_5G", false), 6_000 + D),
        Next::Wait
    );
    assert_eq!(
        t.on_reading(&read("WL_AND_5G", true), 8_000 + D),
        Next::Done
    );
    assert_eq!(end(&t), (Phase::RolledBack, Some(Reason::Timeout)));
}

#[test]
fn rollback_not_through_is_rollback_failed() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 3_000);
    t.on_tick(1_000 + D);
    t.rollback_sent(2_000 + D);
    assert_eq!(
        t.on_reading(&read("WL_AND_5G", false), 2_000 + 2 * D),
        Next::Done
    );
    assert_eq!(
        end(&t),
        (Phase::RollbackFailed, Some(Reason::RollbackTimeout))
    );
}

#[test]
fn third_value_while_rolling_back_is_manual_change() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 3_000);
    t.on_tick(1_000 + D);
    t.rollback_sent(2_000 + D);
    assert_eq!(t.on_reading(&read("Only_5G", true), 4_000 + D), Next::Done);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::ManualChange)));
}

#[test]
fn no_readings_at_all_is_not_judged_not_applied() {
    let mut t = txn(false);
    assert_eq!(t.on_tick(1_000 + D - 1), Next::Wait);
    assert_eq!(t.on_tick(1_000 + D), Next::Done);
    assert_eq!(end(&t), (Phase::Unverified, Some(Reason::NoRollback)));
    let mut t = txn(true);
    assert_eq!(t.on_tick(1_000 + D), Next::Rollback("WL_AND_5G".into()));
}

#[test]
fn sim_change_cancels_in_any_wait() {
    let other = Reading {
        value: Some("Only_LTE".into()),
        registered: true,
        sim: SimId {
            iccid: "89860000000000000002".into(),
            slot: 1,
        },
    };
    let mut t = txn(true);
    assert_eq!(t.on_reading(&other, 3_000), Next::Done);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::SimChanged)));

    let mut t = txn(true);
    t.on_tick(1_000 + D);
    t.rollback_sent(1_000 + D);
    let slot2 = Reading {
        sim: SimId {
            iccid: sim().iccid,
            slot: 2,
        },
        ..read("WL_AND_5G", true)
    };
    assert_eq!(t.on_reading(&slot2, 3_000 + D), Next::Done);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::SimChanged)));
}

#[test]
fn user_revert_works_with_auto_rollback_off() {
    let mut t = txn(false);
    assert_eq!(t.revert(), Ok(Next::Rollback("WL_AND_5G".into())));
    assert_eq!(t.revert(), Err("already rolling back"));
    t.rollback_sent(3_000);
    assert_eq!(t.on_reading(&read("WL_AND_5G", true), 5_000), Next::Done);
    assert_eq!(end(&t), (Phase::RolledBack, Some(Reason::UserRevert)));
    assert_eq!(t.revert(), Err("change already finished"));
}

#[test]
fn keep_confirms_by_user() {
    let mut t = txn(true);
    assert_eq!(t.keep(), Ok(()));
    assert_eq!(end(&t), (Phase::Confirmed, Some(Reason::UserKeep)));
    let mut t = txn(true);
    t.revert().unwrap();
    assert_eq!(t.keep(), Err("already rolling back"));
}

#[test]
fn cancel_only_touches_live_changes() {
    let mut t = txn(true);
    t.cancel(Reason::Superseded);
    assert_eq!(end(&t), (Phase::Cancelled, Some(Reason::Superseded)));
    t.cancel(Reason::Preempted);
    assert_eq!(t.reason, Some(Reason::Superseded));
}

#[test]
fn resume_same_boot_keeps_the_clock() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 60_000);
    assert_eq!(t.resume("boot-a", 70_000), Next::Wait);
    assert_eq!(t.boots, 0);
    assert_eq!(t.wait_start_ms, 1_000);
    assert_eq!(t.on_tick(1_000 + D), Next::Rollback("WL_AND_5G".into()));
}

#[test]
fn first_reboot_restarts_the_clock() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 61_000);
    // 新开机，时钟从头数
    assert_eq!(t.resume("boot-b", 5_000), Next::Wait);
    assert_eq!(t.boots, 1);
    assert_eq!(t.elapsed_prev_ms, 60_000);
    assert_eq!(t.wait_start_ms, 5_000);
    assert_eq!(t.on_tick(5_000 + D - 1), Next::Wait);
    assert_eq!(t.on_tick(5_000 + D), Next::Rollback("WL_AND_5G".into()));
}

#[test]
fn second_reboot_rolls_back_at_once() {
    let mut t = txn(true);
    t.on_reading(&read("Only_LTE", false), 11_000);
    t.resume("boot-b", 5_000);
    t.on_reading(&read("Only_LTE", false), 15_000);
    let next = t.resume("boot-c", 4_000);
    assert_eq!(next, Next::Rollback("WL_AND_5G".into()));
    assert_eq!(t.rollback_reason, Some(Reason::RebootLoop));
    t.rollback_sent(5_000);
    t.on_reading(&read("WL_AND_5G", true), 7_000);
    assert_eq!(end(&t), (Phase::RolledBack, Some(Reason::RebootLoop)));
}

#[test]
fn reboot_after_whole_deadline_rolls_back_at_once() {
    let mut t = txn(true);
    // 上一次开机最后一次看到它时已经等满了时限（那一拍还没来得及判就重启了）
    t.seen_ms = 1_000 + D;
    let next = t.resume("boot-b", 3_000);
    assert_eq!(t.boots, 1);
    assert_eq!(next, Next::Rollback("WL_AND_5G".into()));
    assert_eq!(t.rollback_reason, Some(Reason::RebootLoop));
}

#[test]
fn reboot_loop_with_rollback_off_is_unverified() {
    let mut t = txn(false);
    t.resume("boot-b", 1_000);
    assert_eq!(t.resume("boot-c", 1_000), Next::Done);
    assert_eq!(end(&t), (Phase::Unverified, Some(Reason::NoRollback)));
}

#[test]
fn crash_during_apply_is_never_reapplied() {
    let mut t = Txn::new(
        NewTxn {
            op_id: "op1".into(),
            action: "network.set_mode".into(),
            item: "network.mode".into(),
            source: Source::Screen,
            undo: false,
            target: "Only_LTE".into(),
            old: "WL_AND_5G".into(),
            rollback_to: "WL_AND_5G".into(),
            sim: sim(),
            rollback_enabled: true,
            deadline_ms: D,
            boot_id: "boot-a".into(),
        },
        0,
    );
    t.begin_apply();
    // 意图已落盘，结果没落盘就崩了
    let mut t: Txn = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    assert_eq!(t.resume("boot-a", 9_000), Next::Wait);
    assert_eq!(t.phase, Phase::Verifying);
    assert_eq!(t.intent, None);
    // 读回 = 目标：视为已做
    assert_eq!(t.on_reading(&read("Only_LTE", true), 11_000), Next::Done);
    assert_eq!(end(&t), (Phase::Confirmed, Some(Reason::Verified)));

    // 读回一直是旧值：视为没做，到点 not_applied，不退回
    let mut t: Txn = serde_json::from_str(
        &serde_json::to_string(&{
            let mut x = txn(true);
            x.phase = Phase::Applying;
            x.intent = Some(Intent::Apply);
            x
        })
        .unwrap(),
    )
    .unwrap();
    t.resume("boot-b", 0);
    t.on_reading(&read("WL_AND_5G", true), 2_000);
    assert_eq!(t.on_reading(&read("WL_AND_5G", true), D), Next::Done);
    assert_eq!(end(&t), (Phase::NotApplied, Some(Reason::Ignored)));
}

#[test]
fn crash_during_rollback_never_sends_a_second_one() {
    let mut t = txn(true);
    t.on_tick(1_000 + D);
    assert_eq!(t.intent, Some(Intent::Rollback));
    let mut t: Txn = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    // 同一次开机和整机重启都一样：只接着确认
    let mut u = t.clone();
    assert_eq!(t.resume("boot-a", 2_000 + D), Next::Wait);
    assert_eq!(t.phase, Phase::RollingBack);
    assert_eq!(t.intent, None);
    assert_eq!(u.resume("boot-b", 1_000), Next::Wait);
    assert_eq!(u.phase, Phase::RollingBack);
    assert_eq!(u.on_tick(1_000 + D), Next::Done);
    assert_eq!(
        end(&u),
        (Phase::RollbackFailed, Some(Reason::RollbackTimeout))
    );
}

#[test]
fn status_reports_remaining_time() {
    let t = txn(false);
    let s = t.status(31_000);
    assert_eq!(s["phase"], "verifying");
    assert_eq!(s["source"], "screen");
    assert_eq!(s["remaining_ms"], D - 30_000);
    assert_eq!(s["reason"], Value::Null);
}

#[test]
fn sources() {
    for (s, user) in [
        ("screen", true),
        ("web", true),
        ("legacy", true),
        ("guard", false),
        ("scenario", false),
        ("scheduler", false),
        ("auto", false),
    ] {
        let src = Source::parse(s).unwrap();
        assert_eq!(src.is_user(), user, "{s}");
        assert_eq!(serde_json::to_value(src).unwrap(), s);
    }
    assert!(Source::Guard.may_override());
    assert!(!Source::Scenario.may_override());
    assert_eq!(Source::parse("touch"), None);
}

#[test]
fn sim_check_only_a_different_identity_is_a_change() {
    let blank = SimId {
        iccid: String::new(),
        slot: 0,
    };
    let no_slot = SimId {
        iccid: sim().iccid,
        slot: 0,
    };
    let other = SimId {
        iccid: "89860000000000000002".into(),
        slot: 1,
    };
    assert_eq!(sim_check(&sim(), &sim()), SimCheck::Same);
    assert_eq!(sim_check(&sim(), &other), SimCheck::Changed);
    assert_eq!(sim_check(&sim(), &blank), SimCheck::Unknown);
    assert_eq!(sim_check(&sim(), &no_slot), SimCheck::Unknown);
    assert_eq!(sim_check(&blank, &blank), SimCheck::Same);
    // 写之前没卡、现在插了卡
    assert_eq!(sim_check(&blank, &sim()), SimCheck::Changed);
}

#[test]
fn blank_sim_reading_only_counts_time() {
    let mut t = txn(true);
    let blank = Reading {
        sim: SimId {
            iccid: String::new(),
            slot: 0,
        },
        ..read("WL_AND_5G", true)
    };
    assert_eq!(t.on_reading(&blank, 3_000), Next::Wait);
    assert!(!t.read_ok);
    assert_eq!(t.phase, Phase::Verifying);
}
