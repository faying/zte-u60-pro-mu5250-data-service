//! T4 执行者测试（docs/STATE_V2.md 第 7、8 节）。全部用可控时钟（`start_paused`）和进程内的脚本后端，
//! 后端的延迟、超时都是 `tokio::time::sleep`，不依赖真实时间。

use super::*;
use crate::block::{BlockSpec, Event, EventSink, FAILURE_RETRY};
use crate::ubus::backend::BackendKind;
use serde_json::json;
use std::collections::HashMap;

/// 一次调用的脚本。
#[derive(Clone, Debug)]
enum Step {
    /// 等这么久，回这个值。
    Reply(Value, Duration),
    /// 等这么久，回失败。
    Fail(Duration),
    /// 等这么久（单请求超时），回超时。
    Timeout(Duration),
}

#[derive(Default)]
struct MockInner {
    /// (时间, "对象.方法")
    log: Vec<(Instant, String)>,
    steps: HashMap<String, VecDeque<Step>>,
    defaults: HashMap<String, Step>,
    /// 模拟设备：`set` 写进来，没有脚本的 `list` 读出去。
    device: HashMap<String, Value>,
    in_flight: usize,
    max_in_flight: usize,
    off_executor: usize,
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<MockInner>>);

impl Mock {
    fn default_step(&self, key: &str, step: Step) {
        lock(&self.0).defaults.insert(key.into(), step);
    }
    fn push(&self, key: &str, step: Step) {
        lock(&self.0)
            .steps
            .entry(key.into())
            .or_default()
            .push_back(step);
    }
    fn log(&self) -> Vec<(Instant, String)> {
        lock(&self.0).log.clone()
    }
    fn names(&self) -> Vec<String> {
        self.log().into_iter().map(|(_, k)| k).collect()
    }
    fn times(&self, key: &str) -> Vec<Instant> {
        self.log()
            .into_iter()
            .filter(|(_, k)| k == key)
            .map(|(t, _)| t)
            .collect()
    }
}

impl UbusBackend for Mock {
    fn kind(&self) -> BackendKind {
        BackendKind::Socket
    }
    async fn call(&mut self, object: &str, method: &str, args: &Value) -> Result<Value, UbusError> {
        let key = format!("{object}.{method}");
        let step = {
            let mut m = lock(&self.0);
            m.log.push((Instant::now(), key.clone()));
            m.in_flight += 1;
            m.max_in_flight = m.max_in_flight.max(m.in_flight);
            if !super::on_executor() {
                m.off_executor += 1;
            }
            let scripted = m.steps.get_mut(&key).and_then(VecDeque::pop_front);
            scripted.or_else(|| m.defaults.get(&key).cloned())
        };
        let r = match step {
            Some(Step::Reply(v, d)) => {
                tokio::time::sleep(d).await;
                Ok(v)
            }
            Some(Step::Fail(d)) => {
                tokio::time::sleep(d).await;
                Err(UbusError::Status {
                    object: object.into(),
                    method: method.into(),
                    code: 4,
                })
            }
            Some(Step::Timeout(d)) => {
                tokio::time::sleep(d).await;
                Err(UbusError::Timeout {
                    object: object.into(),
                    detail: format!("ubus {object}: timed out"),
                })
            }
            None => {
                let mut m = lock(&self.0);
                if method == "set" {
                    m.device.insert(object.into(), args.clone());
                    Ok(json!({}))
                } else {
                    Ok(m.device.get(object).cloned().unwrap_or(json!({"ok":1})))
                }
            }
        };
        lock(&self.0).in_flight -= 1;
        r
    }
}

/// 带时间的事件记录。
#[derive(Clone, Default)]
struct Timed(Arc<Mutex<Vec<(Instant, Event)>>>);
impl EventSink for Timed {
    fn emit(&self, event: &Event) {
        lock(&self.0).push((Instant::now(), event.clone()));
    }
}
impl Timed {
    fn heartbeats(&self) -> Vec<(Instant, u64)> {
        lock(&self.0)
            .iter()
            .filter_map(|(t, e)| match e {
                Event::Heartbeat(h) => Some((*t, h.seq)),
                _ => None,
            })
            .collect()
    }
    fn blocks(&self, name: &str) -> Vec<(Instant, crate::block::BlockEvent)> {
        lock(&self.0)
            .iter()
            .filter_map(|(t, e)| match e {
                Event::Block(b) if b.name == name => Some((*t, b.clone())),
                _ => None,
            })
            .collect()
    }
}

fn spec(name: &'static str, interval_s: u64) -> BlockSpec {
    BlockSpec::new(name, name, "list", Duration::from_secs(interval_s))
}

fn setup(specs: Vec<BlockSpec>, cfg: Config, sample_ms: u64) -> (Executor, Mock, Timed) {
    let mock = Mock::default();
    let sink = Timed::default();
    let hub = Arc::new(Hub::new(specs, Box::new(sink.clone())));
    let exec = Executor::spawn(mock.clone(), hub, cfg, Duration::from_millis(sample_ms));
    (exec, mock, sink)
}

/// 没有旧采集的周期采集。
struct NoLegacy;
impl RoundDriver for NoLegacy {
    fn legacy(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// 旧采集也发两个请求（经 `executor::call`）。
struct TwoLegacyCalls;
impl RoundDriver for TwoLegacyCalls {
    fn legacy(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {
            preempt().await;
            let _ = call("legacy", "one", &json!({})).await;
            preempt().await;
            let _ = call("legacy", "two", &json!({})).await;
        })
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

#[tokio::test(start_paused = true)]
async fn executor_is_only_ubus_caller() {
    let (exec, mock, _) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 500);
    mock.default_step(
        "a.list",
        Step::Reply(json!({"v":1}), Duration::from_millis(30)),
    );
    mock.default_step(
        "legacy.one",
        Step::Reply(json!({}), Duration::from_millis(20)),
    );
    mock.default_step("ctl.x", Step::Reply(json!({}), Duration::from_millis(10)));
    mock.default_step("out.y", Step::Reply(json!({}), Duration::from_millis(5)));
    exec.start_rounds(Arc::new(TwoLegacyCalls));
    // 执行者外同时来的调用（登录校验之类）和控制任务。
    let mut handles = Vec::new();
    for i in 0..20u64 {
        let e = exec.clone();
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(37 * i)).await;
            assert!(!on_executor());
            e.call("out", "y", &json!({})).await.unwrap();
        }));
    }
    for i in 0..5u64 {
        let e = exec.clone();
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(111 * i)).await;
            e.control(async {
                call("ctl", "x", &json!({})).await.unwrap();
                call("ctl", "x", &json!({})).await.unwrap();
            })
            .await
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let m = lock(&mock.0);
    assert_eq!(m.max_in_flight, 1, "同一时间最多一个在途");
    assert_eq!(m.off_executor, 0, "每个请求都在执行者任务里发");
    let count = |k: &str| m.log.iter().filter(|(_, n)| n == k).count();
    assert_eq!(count("out.y"), 20);
    assert_eq!(count("ctl.x"), 10);
    assert!(count("a.list") >= 3 && count("legacy.two") >= 3);
    drop(m);

    // 源码里除了执行者和 ubus 模块，没有别的地方能直接起 ubus 后端或 `ubus call`。
    let sources = [
        ("auth.rs", include_str!("../auth.rs")),
        ("control.rs", include_str!("../control.rs")),
        ("server.rs", include_str!("../server.rs")),
        ("sms.rs", include_str!("../sms.rs")),
        ("state.rs", include_str!("../state.rs")),
        ("wifi.rs", include_str!("../wifi.rs")),
        ("extra_wifi.rs", include_str!("../extra_wifi.rs")),
        ("cooling.rs", include_str!("../cooling.rs")),
        ("qos.rs", include_str!("../qos.rs")),
        ("neighbor.rs", include_str!("../neighbor.rs")),
        (
            "neighbor_manager.rs",
            include_str!("../neighbor_manager.rs"),
        ),
        ("block.rs", include_str!("../block.rs")),
        ("main.rs", include_str!("../main.rs")),
    ];
    for (name, src) in sources {
        for needle in [
            "ZWRT_DATAD_UBUS_BIN",
            "CliBackend",
            "SocketBackend",
            "UbusClient",
            "\"/bin/ubus\"",
        ] {
            assert!(!src.contains(needle), "{name} 含 {needle}");
        }
        let spawns = src.matches("Backend::from_env").count();
        assert_eq!(
            spawns,
            usize::from(name == "server.rs"),
            "{name}：后端只在 server.rs 建一次，交给执行者"
        );
    }
    // V2-31：短信事件监听只订阅（`ubus listen`），不发请求。
    let listen = include_str!("../ubus/listen.rs");
    assert!(listen.contains(".args([\"listen\", EVENT])"));
    assert!(!listen.contains("\"call\""), "listen.rs 不能发 ubus call");
    let state = include_str!("../state.rs");
    assert!(
        state.contains("crate::executor::call("),
        "state::ubus 走执行者"
    );
}

#[tokio::test(start_paused = true)]
async fn round_three_timeouts_within_budget_plus_one_timeout() {
    let (exec, mock, sink) = setup(
        vec![spec("a", 0), spec("b", 0), spec("c", 0)],
        Config::default(),
        1000,
    );
    for k in ["a.list", "b.list", "c.list"] {
        mock.default_step(k, Step::Timeout(Duration::from_secs(2)));
    }
    let t0 = Instant::now();
    exec.round_now(async {}).await;
    let took = t0.elapsed();
    // 第 1 个 0～2 秒超时，第 2 个 2～4 秒，第 3 个没开始。
    assert_eq!(mock.names(), ["a.list", "b.list"]);
    assert!(secs(took) >= 4.0 && secs(took) <= 5.0, "{took:?}");
    assert!(took <= ROUND_BUDGET + Duration::from_secs(2));
    // 这一轮的心跳照发。
    let hb = sink.heartbeats();
    assert_eq!(hb.len(), 1);
    assert!(hb[0].0 - t0 <= Duration::from_secs(5));
    // 超时的块读失败，本来就是 stale（从没读成功过），不发 block。
    assert!(sink.blocks("a").is_empty());
}

#[tokio::test(start_paused = true)]
async fn round_rotation_reads_skipped_blocks_first() {
    let (exec, mock, _) = setup(
        vec![spec("a", 0), spec("b", 0), spec("c", 0)],
        Config::default(),
        1000,
    );
    for k in ["a.list", "b.list", "c.list"] {
        mock.default_step(k, Step::Timeout(Duration::from_secs(2)));
    }
    exec.round_now(async {}).await;
    exec.round_now(async {}).await;
    exec.round_now(async {}).await;
    // 第 1 轮 a、b（c 没轮到）；第 2 轮先读 c，再 a（b 没轮到）；第 3 轮先读 b。
    assert_eq!(
        mock.names(),
        ["a.list", "b.list", "c.list", "a.list", "b.list", "c.list"]
    );

    // 慢块不会一直把后面的块挤掉：a 每次都用满预算，b 只在第 1 轮没轮到，之后每轮先读。
    let (exec, mock, _) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 1000);
    mock.default_step(
        "a.list",
        Step::Reply(json!({}), Duration::from_millis(3100)),
    );
    for _ in 0..4 {
        exec.round_now(async {}).await;
    }
    assert_eq!(
        mock.names(),
        [
            "a.list", "b.list", "a.list", "b.list", "a.list", "b.list", "a.list"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn heartbeat_sent_after_over_budget_round() {
    let (exec, mock, sink) = setup(
        vec![spec("a", 0), spec("b", 0), spec("c", 0)],
        Config::default(),
        1000,
    );
    mock.default_step(
        "a.list",
        Step::Reply(json!({"x":1}), Duration::from_millis(3500)),
    );
    let t0 = Instant::now();
    exec.round_now(async {}).await;
    assert_eq!(mock.names(), ["a.list"], "超预算后不再开始新请求");
    let hb = sink.heartbeats();
    assert_eq!(hb.len(), 1);
    assert_eq!(hb[0].0 - t0, Duration::from_millis(3500));
    let seqs: Vec<u64> = lock(&sink.0)
        .iter()
        .map(|(_, e)| match e {
            Event::Block(b) => b.seq,
            Event::Heartbeat(h) => h.seq,
        })
        .collect();
    assert_eq!(seqs, [1, 2], "a 的 block 占 1，心跳占 2");
    match &lock(&sink.0)[1].1 {
        Event::Heartbeat(h) => {
            assert_eq!(h.blocks.len(), 3);
            assert!(h.blocks[0].1 > 0);
            assert_eq!((h.blocks[1].1, h.blocks[2].1), (0, 0));
        }
        e => panic!("{e:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeat_gap_at_most_10s_without_control() {
    // 最坏情况：采样间隔 5 秒、每个对象都卡到 2 秒超时（socket 后端）。
    let (exec, mock, sink) = setup(
        vec![spec("a", 0), spec("b", 0), spec("c", 0), spec("d", 0)],
        Config::default(),
        5000,
    );
    for k in ["a.list", "b.list", "c.list", "d.list"] {
        mock.default_step(k, Step::Timeout(Duration::from_secs(2)));
    }
    let t0 = Instant::now();
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_secs(120)).await;
    let hb = sink.heartbeats();
    assert!(hb.len() >= 10, "{}", hb.len());
    let mut prev = t0;
    for (t, _) in &hb {
        assert!(*t - prev <= Duration::from_secs(10), "{:?}", *t - prev);
        prev = *t;
    }
    // seq 连续（心跳也占号）。
    for w in hb.windows(2) {
        assert!(w[1].1 > w[0].1);
    }
}

#[tokio::test(start_paused = true)]
async fn block_interval_respected() {
    let (exec, mock, _) = setup(
        vec![spec("slow", 5), spec("fast", 0)],
        Config::default(),
        1000,
    );
    let t0 = Instant::now();
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_millis(20_500)).await;
    let slow = mock.times("slow.list");
    let fast = mock.times("fast.list");
    // 每秒一轮：间隔 0 的块每轮读，间隔 5 秒的块满 5 秒才读。
    assert_eq!(fast.len(), 20, "{fast:?}");
    assert_eq!(slow.len(), 4, "{slow:?}");
    assert_eq!(slow[0] - t0, Duration::from_secs(1));
    for w in slow.windows(2) {
        assert_eq!(w[1] - w[0], Duration::from_secs(5));
    }
}

#[tokio::test(start_paused = true)]
async fn block_failure_retried_within_5s() {
    let (exec, mock, sink) = setup(vec![spec("x", 60)], Config::default(), 1000);
    exec.round_now(async {}).await; // 第一次读成功
    mock.push("x.list", Step::Fail(Duration::ZERO));
    exec.mark_immediate(None);
    exec.round_now(async {}).await; // 读失败 → stale
    let failed_at = *mock.times("x.list").last().unwrap();
    assert!(sink.blocks("x").last().unwrap().1.stale);
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_secs(30)).await;
    let times = mock.times("x.list");
    assert_eq!(
        times.len(),
        3,
        "失败后重读一次，之后按 60 秒间隔：{times:?}"
    );
    let retry = times[2] - failed_at;
    assert!(retry <= FAILURE_RETRY, "{retry:?}");
    assert!(!sink.blocks("x").last().unwrap().1.stale, "重读成功翻回");
}

#[tokio::test(start_paused = true)]
async fn block_cache_off_reads_every_round() {
    let cfg = Config {
        cache: false,
        ..Config::default()
    };
    let (exec, mock, _) = setup(vec![spec("x", 60), spec("y", 300)], cfg, 1000);
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    assert_eq!(mock.times("x.list").len(), 10);
    assert_eq!(mock.times("y.list").len(), 10);
}

/// 在执行者正读着第一块（慢块，1 秒）时提交任务。
async fn during_first_block<T: Send + 'static>(
    exec: &Executor,
    jobs: Vec<BoxFuture<'static, T>>,
) -> Vec<Result<T, Busy>> {
    let e = exec.clone();
    let round = tokio::spawn(async move { e.round_now(async {}).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let out = futures_util::future::join_all(jobs.into_iter().map(|j| exec.control(j))).await;
    round.await.unwrap();
    out
}

#[tokio::test(start_paused = true)]
async fn control_runs_before_next_block() {
    let (exec, mock, _) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 1000);
    mock.default_step("a.list", Step::Reply(json!({}), Duration::from_secs(1)));
    let job: BoxFuture<'static, ()> = Box::pin(async {
        call("ctl", "set", &json!({})).await.unwrap();
    });
    during_first_block(&exec, vec![job]).await;
    assert_eq!(mock.names(), ["a.list", "ctl.set", "b.list"]);
}

#[tokio::test(start_paused = true)]
async fn control_queue_ten_requests_eight_ordered_two_busy() {
    let (exec, mock, _) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 1000);
    mock.default_step("a.list", Step::Reply(json!({}), Duration::from_secs(1)));
    let t0 = Instant::now();
    let order = Arc::new(Mutex::new(Vec::new()));
    let jobs: Vec<BoxFuture<'static, (usize, Instant)>> = (0..10usize)
        .map(|i| {
            let order = order.clone();
            Box::pin(async move {
                call("ctl", "set", &json!({"i":i})).await.unwrap();
                lock(&order).push(i);
                (i, Instant::now())
            }) as BoxFuture<'static, _>
        })
        .collect();
    // 结果带上返回的时间：busy 的要立即返回。
    let e = exec.clone();
    let round = tokio::spawn(async move { e.round_now(async {}).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let results = futures_util::future::join_all(jobs.into_iter().map(|j| {
        let e = exec.clone();
        async move {
            let r = e.control(j).await;
            (r, Instant::now())
        }
    }))
    .await;
    round.await.unwrap();
    for (i, (r, at)) in results.iter().enumerate() {
        if i < 8 {
            assert_eq!(r.as_ref().unwrap().0, i);
            assert!(*at - t0 >= Duration::from_secs(1), "排队的等 a 读完才做");
        } else {
            assert_eq!(*r, Err(Busy));
            assert_eq!(*at - t0, Duration::from_millis(100), "满了立即回");
        }
    }
    assert_eq!(*lock(&order), (0..8).collect::<Vec<_>>());
    let names = mock.names();
    assert_eq!(names.first().map(String::as_str), Some("a.list"));
    assert_eq!(names.last().map(String::as_str), Some("b.list"));
    assert_eq!(names.len(), 10);
    // 做完之后队列空了，又能收 8 个。
    let more: Vec<BoxFuture<'static, (usize, Instant)>> = (0..8usize)
        .map(|i| Box::pin(async move { (i, Instant::now()) }) as BoxFuture<'static, _>)
        .collect();
    assert!(
        during_first_block(&exec, more)
            .await
            .iter()
            .all(Result::is_ok)
    );
    // HTTP 层回 503 和文档里的 body。
    let resp = crate::server::control_busy("wifi.set_module");
    assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"ok":false,"action":"wifi.set_module","error":{"code":"busy","message":"control queue full"}})
    );
}

#[tokio::test(start_paused = true)]
async fn control_multi_call_runs_as_one_task() {
    let (exec, mock, _) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 1000);
    mock.default_step("a.list", Step::Reply(json!({}), Duration::from_secs(1)));
    for k in ["apn.mode", "apn.auto", "apn.manu", "apn.enabled"] {
        mock.default_step(k, Step::Reply(json!({}), Duration::from_millis(400)));
    }
    let apn_list: BoxFuture<'static, ()> = Box::pin(async {
        for m in ["mode", "auto", "manu", "enabled"] {
            call("apn", m, &json!({})).await.unwrap();
        }
    });
    let other: BoxFuture<'static, ()> = Box::pin(async {
        call("ctl", "set", &json!({})).await.unwrap();
    });
    during_first_block(&exec, vec![apn_list, other]).await;
    assert_eq!(
        mock.names(),
        [
            "a.list",
            "apn.mode",
            "apn.auto",
            "apn.manu",
            "apn.enabled",
            "ctl.set",
            "b.list"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn control_success_marks_blocks_no_second_round() {
    let (exec, mock, _) = setup(
        vec![spec("battery", 60), spec("charger", 60)],
        Config::default(),
        1000,
    );
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_millis(1500)).await; // 第 1 轮在 1 秒
    assert_eq!(mock.names(), ["battery.list", "charger.list"]);
    let rounds = exec.stats().rounds.load(Ordering::Relaxed);
    let e = exec.clone();
    exec.control(async move {
        call(
            "charger",
            "set",
            &json!({"direct_power_supply_mode":"enable"}),
        )
        .await
        .unwrap();
        e.mark_immediate(Some(&["charger"]));
    })
    .await
    .unwrap();
    // 控制做完，不另起一轮。
    assert!(exec.hub().is_immediate("charger"));
    assert!(!exec.hub().is_immediate("battery"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(exec.stats().rounds.load(Ordering::Relaxed), rounds);
    assert_eq!(mock.names().len(), 3);
    // 下一轮（2 秒）只读被标的块，满 60 秒前不读别的。
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(exec.stats().rounds.load(Ordering::Relaxed), rounds + 1);
    assert_eq!(
        mock.names(),
        [
            "battery.list",
            "charger.list",
            "charger.set",
            "charger.list"
        ]
    );
    assert_eq!(
        exec.hub().view("charger").unwrap().data,
        json!({"direct_power_supply_mode":"enable"})
    );
    // 没有映射：全部块。
    exec.control(async {}).await.unwrap();
    exec.mark_immediate(None);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(mock.names()[4..], ["battery.list", "charger.list"]);
}

#[tokio::test(start_paused = true)]
async fn control_result_not_overwritten_by_older_read() {
    // 充电器先读（旧值），电池读着的时候控制改了设备并标记充电器。
    let (exec, mock, sink) = setup(
        vec![spec("charger", 60), spec("battery", 60)],
        Config::default(),
        1000,
    );
    lock(&mock.0)
        .device
        .insert("charger".into(), json!({"mode":"disable"}));
    mock.push(
        "battery.list",
        Step::Reply(json!({}), Duration::from_secs(1)),
    );
    let e = exec.clone();
    let round = tokio::spawn(async move { e.round_now(async {}).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let e = exec.clone();
    exec.control(async move {
        call("charger", "set", &json!({"mode":"enable"}))
            .await
            .unwrap();
        e.mark_immediate(Some(&["charger"]));
    })
    .await
    .unwrap();
    round.await.unwrap();
    exec.round_now(async {}).await;
    let ev = sink.blocks("charger");
    assert_eq!(
        ev.iter()
            .map(|(_, b)| b.data["mode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["disable", "enable"]
    );
    assert!(ev[1].1.seq > ev[0].1.seq && ev[1].1.revision > ev[0].1.revision);
    assert_eq!(
        exec.hub().legacy("charger").unwrap(),
        json!({"mode":"enable"})
    );
    // 控制插在充电器读之前时，同一轮就读到新值，没有旧值被发出去。
    let (exec, mock, sink) = setup(
        vec![spec("battery", 60), spec("charger", 60)],
        Config::default(),
        1000,
    );
    lock(&mock.0)
        .device
        .insert("charger".into(), json!({"mode":"disable"}));
    mock.push(
        "battery.list",
        Step::Reply(json!({}), Duration::from_secs(1)),
    );
    let job: BoxFuture<'static, ()> = Box::pin(async {
        call("charger", "set", &json!({"mode":"enable"}))
            .await
            .unwrap();
    });
    during_first_block(&exec, vec![job]).await;
    let ev = sink.blocks("charger");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].1.data, json!({"mode":"enable"}));
}

#[tokio::test(start_paused = true)]
async fn legacy_calls_are_preempted_by_control_and_not_budget_cut() {
    let (exec, mock, _) = setup(vec![spec("a", 0)], Config::default(), 1000);
    mock.default_step(
        "a.list",
        Step::Reply(json!({}), Duration::from_millis(3500)),
    );
    mock.default_step("legacy.one", Step::Reply(json!({}), Duration::from_secs(1)));
    let e = exec.clone();
    let round = tokio::spawn(async move {
        e.round_now(async {
            TwoLegacyCalls.legacy().await;
        })
        .await
    });
    // a 读完（3.5 秒，已超预算）之后旧采集照样全读；控制在 legacy.one 和 legacy.two 之间插进来。
    tokio::time::sleep(Duration::from_millis(4000)).await;
    exec.control(async {
        call("ctl", "set", &json!({})).await.unwrap();
    })
    .await
    .unwrap();
    round.await.unwrap();
    assert_eq!(
        mock.names(),
        ["a.list", "legacy.one", "ctl.set", "legacy.two"]
    );
}

#[tokio::test(start_paused = true)]
async fn round_skips_timed_out_object_for_rest_of_round() {
    // R2：块 x 超时后，本轮旧采集里再调 x 直接跳过（算失败、不发请求）；控制任务不受影响；下一轮恢复。
    let (exec, mock, _) = setup(vec![spec("x", 0)], Config::default(), 1000);
    mock.push("x.list", Step::Timeout(Duration::from_secs(2)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let e = exec.clone();
    let round = tokio::spawn(async move {
        e.round_now(async move {
            let r = call("x", "status", &json!({})).await;
            lock(&s).push(r.map_err(|e| e.is_timeout()));
            preempt().await; // 这里插进来的控制任务照常调 x
            let r = call("x", "status", &json!({})).await;
            lock(&s).push(r.map_err(|e| e.is_timeout()));
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;
    exec.control(async { call("x", "set", &json!({})).await.unwrap() })
        .await
        .unwrap();
    round.await.unwrap();
    assert_eq!(*lock(&seen), [Err(true), Err(true)]);
    assert_eq!(mock.names(), ["x.list", "x.set"]);
    exec.round_now(async {}).await;
    assert_eq!(mock.names(), ["x.list", "x.set", "x.list"]);
}

#[tokio::test(start_paused = true)]
async fn set_interval_changes_sleep() {
    let (exec, mock, _) = setup(vec![spec("a", 0)], Config::default(), 5000);
    exec.start_rounds(Arc::new(NoLegacy));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    exec.set_interval_ms(500);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // 正在睡的那次按新间隔重算：0.5 秒时已到期，立刻一轮，之后每 0.5 秒一轮。
    assert_eq!(mock.times("a.list").len(), 3, "{:?}", mock.names());
}

#[tokio::test]
async fn call_without_executor_fails_instead_of_hanging() {
    if GLOBAL.get().is_none() {
        let e = call("a", "b", &json!({})).await.unwrap_err();
        assert!(e.to_string().contains("executor"), "{e}");
    }
}

/// 旧采集把 live 块的数据交进来（和 `state::collect` 一样经 `Hub::record`），每轮一个新值。
struct FeedLive(Arc<Hub>, Arc<AtomicU64>);
impl RoundDriver for FeedLive {
    fn legacy(&self) -> BoxFuture<'static, ()> {
        let (hub, n) = (self.0.clone(), self.1.clone());
        Box::pin(async move {
            let n = n.fetch_add(1, Ordering::Relaxed);
            hub.record("live", Ok(json!({"system":{"uptime":n}})), Instant::now());
        })
    }
}

#[tokio::test(start_paused = true)]
async fn live_block_cadence_follows_round_length() {
    let (exec, mock, sink) = setup(
        vec![
            spec("a", 0),
            BlockSpec::derived("live", Box::new(crate::block::LivePolicy)),
        ],
        Config::default(),
        1000,
    );
    let short = Step::Reply(json!({"x":1}), Duration::from_millis(100));
    mock.default_step("a.list", short.clone());
    // 第 4 轮的 a 很慢：这一轮 3.5 秒（超出预算）。
    for _ in 0..3 {
        mock.push("a.list", short.clone());
    }
    mock.push(
        "a.list",
        Step::Reply(json!({"x":1}), Duration::from_millis(3500)),
    );
    let t0 = Instant::now();
    exec.start_rounds(Arc::new(FeedLive(exec.hub().clone(), Arc::default())));
    tokio::time::sleep(Duration::from_millis(12_000)).await;
    let live = sink.blocks("live");
    let hb = sink.heartbeats();
    // 每轮一次，和心跳一一对应、同一时刻，live 在心跳前一个号。
    assert_eq!(live.len(), hb.len());
    for (l, h) in live.iter().zip(&hb) {
        assert_eq!(l.0, h.0);
        assert_eq!(l.1.seq + 1, h.1);
    }
    // 节拍 = 采样间隔 + 这一轮的长度；慢的那一轮之后不补发。
    let gaps: Vec<u64> = live
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).as_millis() as u64)
        .collect();
    assert_eq!(live[0].0 - t0, Duration::from_millis(1100));
    assert_eq!(&gaps[..4], [1100, 1100, 4500, 1100]);
    assert_eq!(
        live.len(),
        7,
        "12 秒里：1.1、2.2、3.3、7.8、8.9、10.0、11.1"
    );
    assert_eq!(
        live.iter().map(|l| l.1.revision).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7]
    );
}

/// 旧采集里的短信读取：和 `state::ubus_ttl` 一样按 10 秒缓存，读到就记进 `sms` 块；
/// `invalidate` 相当于 `state::invalidate_sms_cache`。
struct SmsLegacy {
    hub: Arc<Hub>,
    last: Arc<Mutex<Option<Instant>>>,
}
impl RoundDriver for SmsLegacy {
    fn legacy(&self) -> BoxFuture<'static, ()> {
        let (hub, last) = (self.hub.clone(), self.last.clone());
        Box::pin(async move {
            preempt().await;
            let fresh = lock(&last).is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            if fresh {
                return;
            }
            let r = call("zwrt_wms", "list", &json!({})).await;
            *lock(&last) = Some(Instant::now());
            hub.record("sms", r.map_err(|e| e.to_string()), Instant::now());
        })
    }
}

/// V2-31：短信事件到达后，不等采样间隔、不等 10 秒列表缓存，下一轮（立即开始）就把新短信读进 sms 块；
/// 一轮开始前的多次事件只多出一轮。
#[tokio::test(start_paused = true)]
async fn sms_event_updates_block_within_one_round() {
    let (exec, mock, _) = setup(
        vec![BlockSpec::derived("sms", Box::new(crate::block::OnChange))],
        Config::default(),
        5000,
    );
    mock.default_step(
        "zwrt_wms.list",
        Step::Reply(json!({"max_id":1}), Duration::from_millis(20)),
    );
    let last = Arc::new(Mutex::new(None));
    exec.start_rounds(Arc::new(SmsLegacy {
        hub: exec.hub().clone(),
        last: last.clone(),
    }));
    tokio::time::sleep(Duration::from_millis(5500)).await; // 第 1 轮在 5 秒
    assert_eq!(exec.hub().view("sms").unwrap().data, json!({"max_id":1}));
    let rounds = exec.stats().rounds.load(Ordering::Relaxed);
    // 新短信到了；事件去抖后：清短信缓存 + 踢执行者。连踢 3 次只算一次。
    mock.default_step(
        "zwrt_wms.list",
        Step::Reply(json!({"max_id":2}), Duration::from_millis(20)),
    );
    let t0 = Instant::now();
    for _ in 0..3 {
        *lock(&last) = None;
        exec.kick(&["sms"]);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(exec.hub().view("sms").unwrap().data, json!({"max_id":2}));
    assert_eq!(exec.stats().rounds.load(Ordering::Relaxed), rounds + 1);
    assert!(Instant::now() - t0 < Duration::from_secs(1));
    assert_eq!(mock.names(), ["zwrt_wms.list", "zwrt_wms.list"]);
    // 之后照常按采样间隔（下一轮在踢的那一轮结束后 5 秒），不再多跑。
    tokio::time::sleep(Duration::from_millis(4000)).await;
    assert_eq!(exec.stats().rounds.load(Ordering::Relaxed), rounds + 1);
}

#[tokio::test(start_paused = true)]
async fn rounds_and_heartbeats_continue_under_control_flood() {
    // 持续灌控制任务和内部任务：每个安全点只做进入时已在排队的，采集轮和心跳照常发生。
    let (exec, mock, sink) = setup(vec![spec("a", 0), spec("b", 0)], Config::default(), 1000);
    mock.default_step(
        "ctl.set",
        Step::Reply(json!({}), Duration::from_millis(200)),
    );
    mock.default_step(
        "int.get",
        Step::Reply(json!({}), Duration::from_millis(100)),
    );
    let t0 = Instant::now();
    exec.start_rounds(Arc::new(NoLegacy));
    let stop = Arc::new(AtomicBool::new(false));
    let mut floods = Vec::new();
    for _ in 0..4 {
        let (e, stop) = (exec.clone(), stop.clone());
        floods.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = e
                    .control(async {
                        call("ctl", "set", &json!({})).await.unwrap();
                    })
                    .await;
            }
        }));
    }
    for _ in 0..2 {
        let (e, stop) = (exec.clone(), stop.clone());
        floods.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = e.call("int", "get", &json!({})).await;
            }
        }));
    }
    tokio::time::sleep(Duration::from_secs(60)).await;
    stop.store(true, Ordering::Relaxed);
    let hb = sink.heartbeats();
    assert!(hb.len() >= 10, "心跳只有 {} 次", hb.len());
    let mut prev = t0;
    for (t, _) in &hb {
        assert!(*t - prev <= Duration::from_secs(10), "{:?}", *t - prev);
        prev = *t;
    }
    assert!(mock.times("a.list").len() >= 10);
    assert!(mock.times("ctl.set").len() >= 100, "控制任务也一直在做");
    for f in floods {
        f.await.unwrap();
    }
}
