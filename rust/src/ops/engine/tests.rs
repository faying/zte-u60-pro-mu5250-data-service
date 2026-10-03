//! 引擎 + 假基带（write-op-layer.md 测试计划 Critical Paths 第 1 条）：可控时钟（`start_paused`），
//! 假基带按脚本接受/忽略/拒绝写，可以换卡、断注册、模拟整机重启（换 boot_id、时钟归零）。

use super::*;
use crate::ops::{
    spec::{NETWORK_MODE, SPECS},
    txn::{Conn, DataPath, SimId},
};
use std::path::PathBuf;
use tokio::time::Instant;

#[derive(Clone, Copy, PartialEq)]
enum Apply {
    /// 写进去，读回马上变。
    Take,
    /// 回成功但读回不变（基带 SSR 后 set_netselect 只记日志不干活）。
    Ignore,
    /// 写调用报错，读回不变。
    Fail,
    /// 写调用一直不回。
    Hang,
    /// 写调用超时，原厂过一会才真改过去（D28）。
    TimeoutThenTake,
}

struct DevState {
    net_select: String,
    registered: bool,
    sim: SimId,
    apply: Apply,
    read_fail: bool,
    writes: Vec<String>,
    reads: usize,
    boot: String,
    clock_base: Instant,
    write_delay: Duration,
    /// 数据通路：应当有数据（None = 读不到）、已连接、连接身份、DNS 探测通不通。
    expected: Option<bool>,
    connected: bool,
    conn: Conn,
    probe_ok: bool,
    /// 每次探测绑定的接口。
    probes: Vec<String>,
}

#[derive(Clone)]
struct Dev(Arc<Mutex<DevState>>);

fn sim(n: u8) -> SimId {
    SimId {
        iccid: format!("8986000000000000000{n}"),
        slot: 1,
    }
}

impl Dev {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(DevState {
            net_select: "WL_AND_5G".into(),
            registered: true,
            sim: sim(1),
            apply: Apply::Take,
            read_fail: false,
            writes: vec![],
            reads: 0,
            boot: "boot-a".into(),
            clock_base: Instant::now(),
            write_delay: Duration::ZERO,
            expected: Some(true),
            connected: true,
            conn: Conn {
                ipv4: "10.0.0.1".into(),
                up_since_ms: Some(1),
            },
            probe_ok: true,
            probes: vec![],
        })))
    }
    fn s(&self) -> MutexGuard<'_, DevState> {
        self.0.lock().unwrap()
    }
    /// 整机重启：新 boot_id，时钟从 0 开始。
    fn reboot(&self, boot: &str) {
        let mut s = self.s();
        s.boot = boot.into();
        s.clock_base = Instant::now();
    }
}

impl Device for Dev {
    async fn read(&self, _spec: &'static Spec) -> Result<Reading, String> {
        let mut s = self.s();
        s.reads += 1;
        if s.read_fail {
            return Err("read failed".into());
        }
        Ok(Reading {
            value: Some(s.net_select.clone()),
            registered: s.registered,
            sim: s.sim.clone(),
            data: Some(DataPath {
                expected: s.expected,
                connected: s.connected,
                conn: s.conn.clone(),
                iface: "rmnet_data0".into(),
                dns: vec!["192.0.2.53".into()],
            }),
            probe: None,
        })
    }
    async fn probe(&self, target: &ProbeTarget) -> Result<(), String> {
        let mut s = self.s();
        s.probes.push(target.iface.clone());
        if s.probe_ok {
            Ok(())
        } else {
            Err("no answer".into())
        }
    }
    async fn write(&self, _spec: &'static Spec, value: &str) -> Result<Value, WriteError> {
        let (apply, delay) = {
            let mut s = self.s();
            s.writes.push(value.into());
            (s.apply, s.write_delay)
        };
        tokio::time::sleep(delay).await;
        match apply {
            Apply::Take => {
                self.s().net_select = value.into();
                Ok(json!({"result":"success"}))
            }
            Apply::Ignore => Ok(json!({"result":"success"})),
            Apply::Fail => Err(WriteError {
                message: "ubus call failed".into(),
                timed_out: false,
            }),
            Apply::Hang => std::future::pending().await,
            Apply::TimeoutThenTake => {
                let d = self.clone();
                let v = value.to_owned();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    d.s().net_select = v;
                });
                Err(WriteError {
                    message: "Request timed out".into(),
                    timed_out: true,
                })
            }
        }
    }
    fn now_ms(&self) -> u64 {
        // 从 1 秒开始，避免 0 当成「没有」
        1_000 + self.s().clock_base.elapsed().as_millis() as u64
    }
    fn boot_id(&self) -> String {
        self.s().boot.clone()
    }
}

const DEADLINE: u64 = 120_000;

fn cfg(rollback: bool) -> Config {
    Config {
        rollback,
        poll: Duration::from_secs(2),
        legacy_ttl_ms: 120_000,
    }
}

fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("datad-ops-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 「进程没了」：把落盘的文件复制到一个新目录（旧引擎的驱动在测试里还活着，会继续往旧目录写）。
fn crashed(dir: &std::path::Path) -> PathBuf {
    let new = temp_dir();
    for f in ["pending.json", "takeover"] {
        if let Ok(b) = std::fs::read(dir.join(f)) {
            std::fs::write(new.join(f), b).unwrap();
        }
    }
    new
}

fn engine(dev: &Dev, rollback: bool, dir: Option<PathBuf>) -> Engine<Dev> {
    Engine::new(dev.clone(), cfg(rollback), Store::open(dir))
}

fn spec() -> &'static Spec {
    &SPECS[0]
}

fn req(target: &str, source: Source) -> Request {
    Request {
        spec: spec(),
        target: target.into(),
        source,
        op_id: None,
        undo: false,
    }
}

fn op_of(s: &Submit) -> Value {
    match s {
        Submit::Applied { op, .. } | Submit::Existing(op) => op.clone(),
        other => panic!("not applied: {other:?}"),
    }
}

fn id(op: &Value) -> String {
    op["op_id"].as_str().unwrap().to_owned()
}

/// 等到这个事务结束（最多 10 分钟假时间），返回最后的状态。
async fn settle(e: &Engine<Dev>, op_id: &str) -> Value {
    for _ in 0..600 {
        let s = e.status(Some(op_id));
        let phase = s["phase"].as_str().unwrap_or_default();
        if !matches!(
            phase,
            "accepted" | "applying" | "verifying" | "rolling_back"
        ) {
            return s;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("{op_id} never finished: {}", e.status(Some(op_id)));
}

fn end(s: &Value) -> (String, String) {
    (
        s["phase"].as_str().unwrap_or_default().into(),
        s["reason"].as_str().unwrap_or_default().into(),
    )
}

fn pair(p: &str, r: &str) -> (String, String) {
    (p.into(), r.into())
}

#[tokio::test(start_paused = true)]
async fn confirms_when_readback_matches_and_registered() {
    let dev = Dev::new();
    let e = engine(&dev, false, None);
    let s = e.submit(req("Only_LTE", Source::Screen)).await;
    let op = op_of(&s);
    assert_eq!(op["phase"], "verifying");
    assert_eq!(op["old"], "WL_AND_5G");
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("confirmed", "verified"));
    assert_eq!(fin["data_ok"], true);
    assert_eq!(dev.s().writes, ["Only_LTE"]);
    // 数据通是在蜂窝接口上探测出来的
    assert_eq!(dev.s().probes, ["rmnet_data0"]);
}

#[tokio::test(start_paused = true)]
async fn connected_but_dns_dead_rolls_back() {
    let dev = Dev::new();
    dev.s().probe_ok = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    let d = dev.clone();
    tokio::spawn(async move {
        // 退回发出去以后旧设置是通的
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut s = d.s();
            if s.writes.len() == 2 {
                s.probe_ok = true;
                break;
            }
        }
    });
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("rolled_back", "timeout"));
    assert_eq!(fin["ever_matched"], true);
    assert_eq!(dev.s().writes, ["Only_LTE", "WL_AND_5G"]);
    // 新设置上：每拍 2 秒，第 2、8、14 秒一轮，隔 30 秒在 44、50、56 秒，再在 86、92、98 秒，
    // 下一轮 128 秒已过 120 秒的时限 → 9 次；退回后再探测一次就通了
    assert_eq!(dev.s().probes.len(), 10);
}

#[tokio::test(start_paused = true)]
async fn roaming_with_roaming_off_confirms_without_a_single_probe() {
    let dev = Dev::new();
    dev.s().expected = Some(false);
    dev.s().connected = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    assert_eq!(
        end(&settle(&e, &id(&op)).await),
        pair("confirmed", "verified")
    );
    assert!(dev.s().probes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unreadable_data_path_does_not_confirm() {
    let dev = Dev::new();
    dev.s().expected = None;
    let e = engine(&dev, false, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(e.status(Some(&id(&op)))["phase"], "verifying");
    dev.s().expected = Some(true);
    assert_eq!(
        end(&settle(&e, &id(&op)).await),
        pair("confirmed", "verified")
    );
    assert!(dev.s().probes.len() == 1);
}

#[tokio::test(start_paused = true)]
async fn waits_for_registration() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Web)).await);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(e.status(Some(&id(&op)))["phase"], "verifying");
    dev.s().registered = true;
    assert_eq!(
        end(&settle(&e, &id(&op)).await),
        pair("confirmed", "verified")
    );
}

#[tokio::test(start_paused = true)]
async fn not_through_with_rollback_off_is_unverified() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, false, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("unverified", "no_rollback"));
    // 自动退回关着：只写了一次
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn not_through_rolls_back_once_and_confirms_the_rollback() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let t0 = Instant::now();
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    // 退回发出后，旧制式能注册上
    let d = dev.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut s = d.s();
            if s.net_select == "WL_AND_5G" {
                s.registered = true;
                break;
            }
        }
    });
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("rolled_back", "timeout"));
    assert_eq!(dev.s().writes, ["Only_LTE", "WL_AND_5G"]);
    assert!(t0.elapsed() >= Duration::from_millis(DEADLINE));
}

#[tokio::test(start_paused = true)]
async fn rollback_that_does_not_come_through_fails_and_stops() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("rollback_failed", "rollback_timeout"));
    tokio::time::sleep(Duration::from_secs(600)).await;
    // 不再自动动它
    assert_eq!(dev.s().writes, ["Only_LTE", "WL_AND_5G"]);
}

#[tokio::test(start_paused = true)]
async fn ignored_write_is_not_applied_without_rollback() {
    let dev = Dev::new();
    dev.s().apply = Apply::Ignore;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("not_applied", "ignored"));
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn failed_write_reports_error_and_ends_quickly() {
    let dev = Dev::new();
    dev.s().apply = Apply::Fail;
    let e = engine(&dev, true, None);
    let t0 = Instant::now();
    let s = e.submit(req("Only_LTE", Source::Screen)).await;
    let Submit::Applied { result, op } = s else {
        panic!("{s:?}")
    };
    assert_eq!(result, Err("ubus call failed".into()));
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("not_applied", "ignored"));
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn third_value_mid_wait_is_manual_change() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    dev.s().net_select = "Only_5G".into();
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("cancelled", "manual_change"));
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn sim_change_abandons_without_writing_old_value() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    dev.s().sim = sim(2);
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("cancelled", "sim_changed"));
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn other_writers_get_busy_with_who_and_what() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let first = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    for src in [Source::Scenario, Source::Scheduler, Source::Auto] {
        match e.submit(req("Only_5G", src)).await {
            Submit::Busy(d) => {
                assert_eq!(d["op_id"], first["op_id"]);
                assert_eq!(d["source"], "screen");
                assert_eq!(d["action"], "network.set_mode");
                assert!(d["age_ms"].as_u64().unwrap() >= 3_000);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn same_item_user_write_supersedes_and_inherits_rollback_target() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let b = op_of(&e.submit(req("Only_5G", Source::Web)).await);
    assert_eq!(
        end(&e.status(Some(&id(&a)))),
        pair("cancelled", "superseded")
    );
    assert_eq!(b["old"], "Only_LTE");
    // D35：退回到最后确认过的值，不是没确认的中间值
    assert_eq!(b["rollback_to"], "WL_AND_5G");
    let fin = settle(&e, &id(&b)).await;
    assert_eq!(fin["phase"], "rollback_failed");
    assert_eq!(dev.s().writes, ["Only_LTE", "Only_5G", "WL_AND_5G"]);
}

#[tokio::test(start_paused = true)]
async fn guard_may_override_its_own_item() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let b = op_of(&e.submit(req("WL_AND_5G", Source::Guard)).await);
    assert_eq!(e.status(Some(&id(&a)))["reason"], "superseded");
    assert_eq!(b["source"], "guard");
}

#[tokio::test(start_paused = true)]
async fn write_in_flight_cannot_be_overridden() {
    let dev = Dev::new();
    dev.s().write_delay = Duration::from_secs(5);
    let e = engine(&dev, true, None);
    let e2 = e.clone();
    let first = tokio::spawn(async move { e2.submit(req("Only_LTE", Source::Screen)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    match e.submit(req("Only_5G", Source::Screen)).await {
        Submit::Busy(d) => assert_eq!(d["phase"], "applying"),
        other => panic!("{other:?}"),
    }
    first.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn user_revert_and_keep() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, false, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    dev.s().registered = true;
    // 自动退回关着，「立即退回」照样能用
    let v = e.revert(&id(&a)).unwrap();
    assert_eq!(v["phase"], "rolling_back");
    assert_eq!(v["rollback_reason"], "user_revert");
    assert_eq!(
        end(&settle(&e, &id(&a)).await),
        pair("rolled_back", "user_revert")
    );
    assert_eq!(dev.s().writes, ["Only_LTE", "WL_AND_5G"]);
    assert!(e.revert(&id(&a)).is_err());

    dev.s().registered = false;
    let b = op_of(&e.submit(req("Only_LTE", Source::Web)).await);
    let v = e.keep(&id(&b)).unwrap();
    assert_eq!(end(&v), pair("confirmed", "user_keep"));
    assert!(e.keep(&id(&b)).is_err());
}

#[tokio::test(start_paused = true)]
async fn op_id_is_idempotent() {
    let dev = Dev::new();
    let e = engine(&dev, false, None);
    let mut r = req("Only_LTE", Source::Screen);
    r.op_id = Some("web-1".into());
    let a = op_of(&e.submit(r).await);
    assert_eq!(a["op_id"], "web-1");
    let mut r = req("Only_LTE", Source::Screen);
    r.op_id = Some("web-1".into());
    assert!(matches!(e.submit(r).await, Submit::Existing(_)));
    settle(&e, "web-1").await;
    let mut r = req("Only_5G", Source::Screen);
    r.op_id = Some("web-1".into());
    let again = op_of(&e.submit(r).await);
    assert_eq!(again["target"], "Only_LTE");
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn capture_failure_does_not_touch_the_device() {
    let dev = Dev::new();
    dev.s().read_fail = true;
    let e = engine(&dev, true, None);
    assert!(matches!(
        e.submit(req("Only_LTE", Source::Screen)).await,
        Submit::NoCapture(_)
    ));
    assert!(dev.s().writes.is_empty());
    // 锁没被占着
    dev.s().read_fail = false;
    op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
}

#[tokio::test(start_paused = true)]
async fn safety_write_preempts_and_clears_legacy_queue() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    e.lock().legacy.push_back(LegacyReq {
        item: NETWORK_MODE,
        action: "network.set_mode".into(),
        params: json!({"mode":"Only_5G"}),
        at_ms: e.now(),
    });
    assert_eq!(e.legacy_gate(None, true), LegacyGate::Pass);
    assert_eq!(
        end(&e.status(Some(&id(&a)))),
        pair("cancelled", "preempted")
    );
    assert!(e.lock().legacy.is_empty());
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

fn recording_runner(e: &Engine<Dev>, busy_first: usize) -> Arc<Mutex<Vec<(String, Value)>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let left = Arc::new(Mutex::new(busy_first));
    e.set_legacy_runner(Arc::new(move |action, params| {
        let c = c.clone();
        let left = left.clone();
        Box::pin(async move {
            let mut l = left.lock().unwrap();
            if *l > 0 {
                *l -= 1;
                return false;
            }
            c.lock().unwrap().push((action, params));
            true
        })
    }));
    calls
}

#[tokio::test(start_paused = true)]
async fn legacy_requests_queue_while_a_write_is_in_flight_and_run_after() {
    let dev = Dev::new();
    dev.s().write_delay = Duration::from_secs(5);
    let e = engine(&dev, true, None);
    let calls = recording_runner(&e, 0);
    let e2 = e.clone();
    let first = tokio::spawn(async move { e2.submit(req("Only_LTE", Source::Screen)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    // 旧请求：同一项但写还在途 → 排队；只留最新的
    assert_eq!(e.legacy_gate(Some(NETWORK_MODE), false), LegacyGate::Queue);
    e.enqueue_legacy(NETWORK_MODE, "network.set_mode", json!({"mode":"Only_5G"}));
    e.enqueue_legacy(
        NETWORK_MODE,
        "network.set_mode",
        json!({"mode":"WL_AND_5G"}),
    );
    assert!(calls.lock().unwrap().is_empty());
    let op = op_of(&first.await.unwrap());
    settle(&e, &id(&op)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        [("network.set_mode".to_string(), json!({"mode":"WL_AND_5G"}))]
    );
}

#[tokio::test(start_paused = true)]
async fn legacy_same_item_while_verifying_supersedes() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(e.legacy_gate(Some(NETWORK_MODE), false), LegacyGate::Pass);
    assert_eq!(e.status(Some(&id(&a)))["reason"], "superseded");
    // 不在描述表里的动作照旧
    assert_eq!(e.legacy_gate(None, false), LegacyGate::Pass);
}

#[tokio::test(start_paused = true)]
async fn legacy_queue_retries_when_executor_is_full_and_expires() {
    let dev = Dev::new();
    let e = engine(&dev, true, None);
    let calls = recording_runner(&e, 2);
    e.enqueue_legacy(NETWORK_MODE, "network.set_mode", json!({"mode":"Only_LTE"}));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(calls.lock().unwrap().len(), 1);

    // 锁一直被占着：120 秒后丢掉
    let dev = Dev::new();
    dev.s().registered = false;
    let e = Engine::new(
        dev.clone(),
        Config {
            legacy_ttl_ms: 60_000,
            ..cfg(false)
        },
        Store::open(None),
    );
    let calls = recording_runner(&e, 0);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    e.enqueue_legacy(NETWORK_MODE, "network.set_mode", json!({"mode":"Only_5G"}));
    settle(&e, &id(&a)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(calls.lock().unwrap().is_empty());
    assert!(e.lock().legacy.is_empty());
}

// ---- 落盘与续跑 ----

#[tokio::test(start_paused = true)]
async fn no_pending_file_means_no_device_reads_at_start() {
    let dev = Dev::new();
    let e = engine(&dev, true, Some(temp_dir()));
    e.start().await;
    assert_eq!(dev.s().reads, 0);
    assert_eq!(e.status(None), Value::Null);
}

#[tokio::test(start_paused = true)]
async fn finished_change_leaves_no_pending_file() {
    let dir = temp_dir();
    let dev = Dev::new();
    let e = engine(&dev, false, Some(dir.clone()));
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    assert!(dir.join("pending.json").exists());
    settle(&e, &id(&op)).await;
    assert!(!dir.join("pending.json").exists());
}

#[tokio::test(start_paused = true)]
async fn crash_mid_apply_resumes_without_reapplying() {
    let dir = temp_dir();
    let dev = Dev::new();
    dev.s().apply = Apply::Hang;
    let e = engine(&dev, true, Some(dir.clone()));
    let e2 = e.clone();
    tokio::spawn(async move { e2.submit(req("Only_LTE", Source::Screen)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let saved: Txn =
        serde_json::from_slice(&std::fs::read(dir.join("pending.json")).unwrap()).unwrap();
    assert_eq!(saved.phase, Phase::Applying);

    // datad 被杀、拉起：新引擎，同一个目录。设备其实已经切过去了。
    let dev2 = Dev::new();
    dev2.s().net_select = "Only_LTE".into();
    let e3 = engine(&dev2, true, Some(dir.clone()));
    e3.start().await;
    let fin = settle(&e3, &saved.op_id).await;
    assert_eq!(end(&fin), pair("confirmed", "verified"));
    assert!(dev2.s().writes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn crash_mid_rollback_never_sends_a_second_rollback() {
    let dir = temp_dir();
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, Some(dir.clone()));
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    // 退回的写一直不回：停在「退回意图已落盘」
    tokio::time::sleep(Duration::from_secs(60)).await;
    dev.s().apply = Apply::Hang;
    tokio::time::sleep(Duration::from_secs(70)).await;
    let saved: Txn =
        serde_json::from_slice(&std::fs::read(dir.join("pending.json")).unwrap()).unwrap();
    assert_eq!(saved.phase, Phase::RollingBack);
    assert_eq!(saved.intent, Some(crate::ops::txn::Intent::Rollback));

    for boot in ["boot-a", "boot-b"] {
        let dir = crashed(&dir);
        let dev2 = Dev::new();
        dev2.reboot(boot);
        dev2.s().net_select = "WL_AND_5G".into();
        let e2 = engine(&dev2, true, Some(dir.clone()));
        e2.start().await;
        let fin = settle(&e2, &id(&op)).await;
        assert_eq!(end(&fin), pair("rolled_back", "timeout"), "{boot}");
        assert!(dev2.s().writes.is_empty(), "{boot}");
    }
}

#[tokio::test(start_paused = true)]
async fn reboot_restarts_the_clock_then_second_reboot_rolls_back() {
    let dir = temp_dir();
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, Some(dir.clone()));
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let dir1 = crashed(&dir);

    // 第 1 次开机：接着等，时限从开机重新计
    let dev1 = Dev::new();
    dev1.reboot("boot-b");
    dev1.s().net_select = "Only_LTE".into();
    dev1.s().registered = false;
    let e1 = engine(&dev1, true, Some(dir1.clone()));
    e1.start().await;
    let s = e1.status(Some(&id(&op)));
    assert_eq!(s["phase"], "verifying");
    assert_eq!(s["remaining_ms"], DEADLINE);
    assert!(dev1.s().writes.is_empty());
    tokio::time::sleep(Duration::from_secs(20)).await;
    let dir2 = crashed(&dir1);
    let t1: Txn =
        serde_json::from_slice(&std::fs::read(dir2.join("pending.json")).unwrap()).unwrap();
    assert_eq!(t1.boots, 1);

    // 第 2 次开机仍未确认：开机马上退回
    let dev2 = Dev::new();
    dev2.reboot("boot-c");
    dev2.s().net_select = "Only_LTE".into();
    let e2 = engine(&dev2, true, Some(dir2.clone()));
    e2.start().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(dev2.s().writes, ["WL_AND_5G"]);
    let fin = settle(&e2, &id(&op)).await;
    assert_eq!(end(&fin), pair("rolled_back", "reboot_loop"));
}

#[tokio::test(start_paused = true)]
async fn takeover_marker_abandons_the_pending_change() {
    let dir = temp_dir();
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, Some(dir.clone()));
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    std::fs::write(dir.join("takeover"), b"").unwrap();
    let dir = crashed(&dir);

    let dev2 = Dev::new();
    let e2 = engine(&dev2, true, Some(dir.clone()));
    e2.start().await;
    assert_eq!(
        end(&e2.status(Some(&id(&op)))),
        pair("cancelled", "takeover")
    );
    assert!(!dir.join("takeover").exists());
    assert!(!dir.join("pending.json").exists());
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(dev2.s().reads, 0);
    assert!(dev2.s().writes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn unreadable_pending_file_is_set_aside() {
    let dir = temp_dir();
    std::fs::write(dir.join("pending.json"), b"{not json").unwrap();
    let dev = Dev::new();
    let e = engine(&dev, true, Some(dir.clone()));
    e.start().await;
    assert!(dir.join("pending.json.bad").exists());
    assert_eq!(dev.s().reads, 0);
}

#[test]
fn op_ids() {
    assert!(valid_op_id("web-1.a_B"));
    assert!(!valid_op_id(""));
    assert!(!valid_op_id("a b"));
    assert!(!valid_op_id(&"x".repeat(65)));
}

// ---- D32：退回之前也核对 SIM；身份临时读空不算换卡 ----

#[tokio::test(start_paused = true)]
async fn second_boot_with_another_sim_does_not_roll_back() {
    let dir = temp_dir();
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, Some(dir.clone()));
    let op = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut t: Txn =
        serde_json::from_slice(&std::fs::read(dir.join("pending.json")).unwrap()).unwrap();
    t.boots = 1;
    let dir = crashed(&dir);
    std::fs::write(dir.join("pending.json"), serde_json::to_vec(&t).unwrap()).unwrap();

    // 第 2 次开机，卡换了：不往新卡上写旧卡的值
    let dev2 = Dev::new();
    dev2.reboot("boot-c");
    dev2.s().sim = sim(2);
    let e2 = engine(&dev2, true, Some(dir.clone()));
    e2.start().await;
    let fin = settle(&e2, &id(&op)).await;
    assert_eq!(end(&fin), pair("cancelled", "sim_changed"));
    assert!(dev2.s().writes.is_empty());
}

#[tokio::test(start_paused = true)]
async fn revert_after_sim_change_does_not_write() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, false, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    // 读不到设备的那几拍里换了卡
    dev.s().read_fail = true;
    tokio::time::sleep(Duration::from_secs(5)).await;
    dev.s().sim = sim(2);
    e.revert(&id(&a)).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    dev.s().read_fail = false;
    let fin = settle(&e, &id(&a)).await;
    assert_eq!(end(&fin), pair("cancelled", "sim_changed"));
    assert_eq!(dev.s().writes, ["Only_LTE"]);
}

#[tokio::test(start_paused = true)]
async fn sim_identity_blank_for_a_while_is_not_a_sim_change() {
    let dev = Dev::new();
    dev.s().registered = false;
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    // 重新注册时 ICCID 和卡槽临时读空两拍
    dev.s().sim = SimId {
        iccid: String::new(),
        slot: 0,
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(e.status(Some(&id(&a)))["phase"], "verifying");
    {
        let mut s = dev.s();
        s.sim = sim(1);
        s.registered = true;
    }
    assert_eq!(
        end(&settle(&e, &id(&a)).await),
        pair("confirmed", "verified")
    );
}

#[tokio::test(start_paused = true)]
async fn no_sim_device_still_confirms() {
    let dev = Dev::new();
    dev.s().sim = SimId {
        iccid: String::new(),
        slot: 0,
    };
    let e = engine(&dev, true, None);
    let a = op_of(&e.submit(req("Only_LTE", Source::Screen)).await);
    assert_eq!(
        end(&settle(&e, &id(&a)).await),
        pair("confirmed", "verified")
    );
}

#[tokio::test(start_paused = true)]
async fn timed_out_write_is_unknown_and_confirmed_by_readback() {
    let dev = Dev::new();
    dev.s().apply = Apply::TimeoutThenTake;
    let e = engine(&dev, true, None);
    let s = e.submit(req("Only_LTE", Source::Screen)).await;
    let Submit::Applied { result, op } = s else {
        panic!("{s:?}")
    };
    assert_eq!(result, Err("Request timed out".into()));
    assert_eq!(op["phase"], "verifying");
    // 前几拍读回还是旧值：不判 not_applied
    let fin = settle(&e, &id(&op)).await;
    assert_eq!(end(&fin), pair("confirmed", "verified"));
    assert_eq!(fin["data_ok"], true);
    assert_eq!(dev.s().writes, ["Only_LTE"]);
    // 数据通是在蜂窝接口上探测出来的
    assert_eq!(dev.s().probes, ["rmnet_data0"]);
}
