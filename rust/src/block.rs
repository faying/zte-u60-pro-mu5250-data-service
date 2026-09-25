//! 块模型（docs/STATE_V2.md 第 5、6 节）：每块的 revision、stale、max_age、采集间隔，
//! 以及 `/v2` 事件的 seq 分配点。
//!
//! - `Hub` 持有全部块和全局 `seq`，都在同一把锁里（V2-5）。执行者（`executor.rs`）读完一块调
//!   `Hub::record_read`，每轮结束调 `Hub::round_end`（max_age 检查、策略的轮末钩子、心跳）。
//! - 事件交给 `EventSink::emit`，在锁里调用、`seq` 已经分配好。T5 在这里接 broadcast
//!   （序列化一次、`send`），`/v2` 的 snapshot 也在同一把锁里拍（`Hub::snapshot`）。
//! - 发布时机由每块的 `PublishPolicy` 决定（扩展点）：默认 `OnChange`（V2-17），
//!   T5 的信号块（V2-15，阈值 + 30 秒）和 live 块（V2-16，每轮结束）各实现一个。
//!   不管策略怎么说，revision 只在「数据或 stale 变了」时 +1（V2-11），stale 翻转总是立即发布（V2-12）。
//! - 旧接口从 `Hub::legacy` 取数据：stale 的块按读失败处理（V2-29）。

use serde_json::Value;
use std::{
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

/// V2-13：max_age 的下限。
pub const MIN_MAX_AGE: Duration = Duration::from_secs(15);
/// V2-21：读失败的块多久内重读。
pub const FAILURE_RETRY: Duration = Duration::from_secs(5);

/// 一块读什么、多久读一次、怎么发布。
pub struct BlockSpec {
    pub name: &'static str,
    pub object: &'static str,
    pub method: &'static str,
    pub args: Value,
    /// 采集间隔；0 = 每轮（V2-21）。
    pub interval: Duration,
    pub policy: Box<dyn PublishPolicy>,
}

impl BlockSpec {
    pub fn new(
        name: &'static str,
        object: &'static str,
        method: &'static str,
        interval: Duration,
    ) -> Self {
        Self {
            name,
            object,
            method,
            args: Value::Object(Default::default()),
            interval,
            policy: Box::new(OnChange),
        }
    }
}

/// 阶段 1 迁到块模型的块：电池、充电器。间隔沿用 `ubus_ttl` 的 5 秒。
pub fn phase1_blocks() -> Vec<BlockSpec> {
    vec![
        BlockSpec::new(
            "battery",
            "zwrt_bsp.battery",
            "list",
            Duration::from_secs(5),
        ),
        BlockSpec::new(
            "charger",
            "zwrt_bsp.charger",
            "list",
            Duration::from_secs(5),
        ),
    ]
}

/// 策略的回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// 这次不发（stale 翻转除外）。
    No,
    /// 数据和上次发布的不同才发，发了 revision +1。
    IfChanged,
}

/// 策略看到的东西（默认策略用不到；T5 的信号块、live 块用）。
#[allow(dead_code)]
pub struct PolicyInput<'a> {
    /// 上次发布的数据（从没发布过是 None）。
    pub published: Option<&'a Value>,
    /// 最新一次读成功的数据。
    pub current: Option<&'a Value>,
    /// 距上次发布多久（从没发布过是 None）。
    pub since_publish: Option<Duration>,
    pub now: Instant,
}

/// 发布策略（扩展点）。只管「什么时候考虑发」；revision 和 stale 的规则由 `Hub` 统一执行。
pub trait PublishPolicy: Send {
    /// 读成功之后。
    fn on_read(&mut self, _input: &PolicyInput<'_>) -> Publish {
        Publish::IfChanged
    }
    /// 每轮结束（V2-16 的 live 块在这里发）。
    fn on_round_end(&mut self, _input: &PolicyInput<'_>) -> Publish {
        Publish::No
    }
}

/// V2-17：数据有任何变化就发。
pub struct OnChange;
impl PublishPolicy for OnChange {}

/// 一条 `block` 事件（V2-10）。`data` 是发布出去的值，从没读成功过是 null。
#[derive(Debug, Clone, PartialEq)]
pub struct BlockEvent {
    pub seq: u64,
    pub name: &'static str,
    pub revision: u64,
    pub observed_at: u64,
    pub stale: bool,
    pub data: Value,
}

/// 一条 `heartbeat` 事件：每块的 observed_at（从没读成功过是 0）。
#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub seq: u64,
    pub blocks: Vec<(&'static str, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Block(BlockEvent),
    Heartbeat(Heartbeat),
}

/// 事件出口。在 `Hub` 的锁里调用，`seq` 已分配；不要在这里阻塞或回调 `Hub`。
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event);
}

/// 阶段 1 没有 `/v2` 订阅方（T5 接上）：丢掉。
pub struct NoSink;
impl EventSink for NoSink {
    fn emit(&self, _event: &Event) {}
}

/// `/v2` 看到的一块（snapshot 用）。
#[derive(Debug, Clone, PartialEq)]
pub struct BlockView {
    pub name: &'static str,
    pub revision: u64,
    pub observed_at: u64,
    pub stale: bool,
    pub data: Value,
}

struct Block {
    spec: BlockSpec,
    revision: u64,
    observed_at: u64,
    stale: bool,
    /// 最近一次读成功的值（stale 时保留）。
    data: Option<Value>,
    /// 上次发布的值。
    published: Option<Value>,
    published_at: Option<Instant>,
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    last_failed: bool,
    /// `/control` 之后「下一轮立即读」（V2-21、V2-27）。
    immediate: bool,
}

impl Block {
    fn new(spec: BlockSpec) -> Self {
        Self {
            spec,
            revision: 0,
            observed_at: 0,
            stale: true,
            data: None,
            published: None,
            published_at: None,
            last_attempt: None,
            last_success: None,
            last_failed: false,
            immediate: false,
        }
    }

    fn view(&self) -> BlockView {
        BlockView {
            name: self.spec.name,
            revision: self.revision,
            observed_at: self.observed_at,
            stale: self.stale,
            data: self.published.clone().unwrap_or(Value::Null),
        }
    }

    /// 采集间隔为 0 的块按当前采样间隔算（V2-13）。
    fn max_age(&self, sample_interval: Duration) -> Duration {
        let base = if self.spec.interval.is_zero() {
            sample_interval
        } else {
            self.spec.interval
        };
        (base * 3).max(MIN_MAX_AGE)
    }
}

struct Inner {
    seq: u64,
    blocks: Vec<Block>,
}

/// 采集调度的开关（`executor::Config` 里的同名字段）。
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    /// `ZWRT_DATAD_CACHE` 没设成 0。关掉时每块每轮都读。
    pub cache: bool,
    pub sample_interval: Duration,
}

pub struct Hub {
    inner: Mutex<Inner>,
    sink: Box<dyn EventSink>,
}

fn wall_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Hub {
    pub fn new(specs: Vec<BlockSpec>, sink: Box<dyn EventSink>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seq: 0,
                blocks: specs.into_iter().map(Block::new).collect(),
            }),
            sink,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().blocks.len()
    }

    /// 第 `i` 块的请求（对象、方法、参数）。
    pub fn request(&self, i: usize) -> (&'static str, &'static str, Value) {
        let g = self.lock();
        let s = &g.blocks[i].spec;
        (s.object, s.method, s.args.clone())
    }

    /// 这一刻该不该读第 `i` 块（V2-21）。
    pub fn due(&self, i: usize, now: Instant, sched: Schedule) -> bool {
        let g = self.lock();
        let b = &g.blocks[i];
        let Some(last) = b.last_attempt else {
            return true;
        };
        if b.immediate || !sched.cache || b.spec.interval.is_zero() {
            return true;
        }
        let elapsed = now.saturating_duration_since(last);
        if b.last_failed {
            // 失败的块 5 秒内重读：下一轮若会超过 5 秒，这一轮就读。
            let retry = FAILURE_RETRY
                .saturating_sub(sched.sample_interval)
                .min(b.spec.interval);
            return elapsed >= retry;
        }
        elapsed >= b.spec.interval
    }

    /// 把块标成「下一轮立即读」。`names` 为 None（没有映射）时标全部块（R12）。
    pub fn mark_immediate(&self, names: Option<&[&str]>) {
        let mut g = self.lock();
        for b in &mut g.blocks {
            if names.is_none_or(|n| n.contains(&b.spec.name)) {
                b.immediate = true;
            }
        }
    }

    #[cfg(test)]
    pub fn is_immediate(&self, name: &str) -> bool {
        self.lock()
            .blocks
            .iter()
            .any(|b| b.spec.name == name && b.immediate)
    }

    /// 读完第 `i` 块。`Ok` 必须是 JSON 对象，否则算读失败（返回无效数据）。
    pub fn record_read(&self, i: usize, result: Result<Value, String>, now: Instant) {
        let mut g = self.lock();
        let g = &mut *g;
        let b = &mut g.blocks[i];
        b.last_attempt = Some(now);
        b.immediate = false;
        match result {
            Ok(v) if v.is_object() => {
                b.last_failed = false;
                b.last_success = Some(now);
                b.observed_at = wall_secs();
                b.data = Some(v);
                let was_stale = b.stale;
                b.stale = false;
                let input = PolicyInput {
                    published: b.published.as_ref(),
                    current: b.data.as_ref(),
                    since_publish: b.published_at.map(|t| now.saturating_duration_since(t)),
                    now,
                };
                let decision = b.spec.policy.on_read(&input);
                Self::publish(&mut g.seq, b, &*self.sink, decision, was_stale, now);
            }
            _ => {
                b.last_failed = true;
                if !b.stale {
                    // V2-12：读失败立即 stale；数据保留。
                    b.stale = true;
                    Self::publish(&mut g.seq, b, &*self.sink, Publish::No, true, now);
                }
            }
        }
    }

    /// 发布一块：`flipped` 表示 stale 刚翻转（必须发）。数据或健康变了才涨 revision、发事件。
    fn publish(
        seq: &mut u64,
        b: &mut Block,
        sink: &dyn EventSink,
        decision: Publish,
        flipped: bool,
        now: Instant,
    ) {
        let data_changed = b.published != b.data;
        let data_goes_out = match decision {
            Publish::IfChanged => data_changed,
            // stale 翻转成 false 时，新值跟着发出去（不能发一条 stale=false 却带着旧值）。
            Publish::No => flipped && !b.stale && data_changed,
        };
        if !flipped && !data_goes_out {
            return;
        }
        if data_goes_out {
            b.published = b.data.clone();
        }
        b.published_at = Some(now);
        b.revision += 1;
        *seq += 1;
        let v = b.view();
        sink.emit(&Event::Block(BlockEvent {
            seq: *seq,
            name: v.name,
            revision: v.revision,
            observed_at: v.observed_at,
            stale: v.stale,
            data: v.data,
        }));
    }

    /// 每轮结束（V2-13、V2-16、V2-22）：超过 max_age 的块置 stale；各块策略的轮末钩子；最后发心跳。
    pub fn round_end(&self, now: Instant, sample_interval: Duration) -> Heartbeat {
        let mut g = self.lock();
        let g = &mut *g;
        for b in &mut g.blocks {
            if !b.stale
                && let Some(ok) = b.last_success
                && now.saturating_duration_since(ok) > b.max_age(sample_interval)
            {
                b.stale = true;
                Self::publish(&mut g.seq, b, &*self.sink, Publish::No, true, now);
                continue;
            }
            if b.stale {
                continue;
            }
            let input = PolicyInput {
                published: b.published.as_ref(),
                current: b.data.as_ref(),
                since_publish: b.published_at.map(|t| now.saturating_duration_since(t)),
                now,
            };
            let decision = b.spec.policy.on_round_end(&input);
            if decision != Publish::No {
                Self::publish(&mut g.seq, b, &*self.sink, decision, false, now);
            }
        }
        g.seq += 1;
        let hb = Heartbeat {
            seq: g.seq,
            blocks: g
                .blocks
                .iter()
                .map(|b| (b.spec.name, b.observed_at))
                .collect(),
        };
        self.sink.emit(&Event::Heartbeat(hb.clone()));
        hb
    }

    /// 切点 `seq` 和全部块（V2-4；T5 在同一把锁里先订阅再调它）。
    #[allow(dead_code)] // T5 的 /v2/state、snapshot 用
    pub fn snapshot(&self) -> (u64, Vec<BlockView>) {
        let g = self.lock();
        (g.seq, g.blocks.iter().map(Block::view).collect())
    }

    #[allow(dead_code)] // T5 用；测试用
    pub fn view(&self, name: &str) -> Option<BlockView> {
        self.lock()
            .blocks
            .iter()
            .find(|b| b.spec.name == name)
            .map(Block::view)
    }

    /// 旧接口用（V2-29）：stale（含从没读成功）按读失败处理，不给保留的旧值。
    pub fn legacy(&self, name: &str) -> Result<Value, String> {
        let g = self.lock();
        match g.blocks.iter().find(|b| b.spec.name == name) {
            Some(b) if !b.stale => b.data.clone().ok_or_else(|| "no data".into()),
            Some(_) => Err(format!("block {name} is stale")),
            None => Err(format!("no block {name}")),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// 记下所有事件。
    #[derive(Default, Clone)]
    pub(crate) struct Recorder(pub Arc<Mutex<Vec<Event>>>);
    impl EventSink for Recorder {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }
    impl Recorder {
        pub(crate) fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
        pub(crate) fn blocks(&self, name: &str) -> Vec<BlockEvent> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    Event::Block(b) if b.name == name => Some(b),
                    _ => None,
                })
                .collect()
        }
    }

    fn make_hub(interval_s: u64) -> (Hub, Recorder) {
        let rec = Recorder::default();
        let hub = Hub::new(
            vec![BlockSpec::new(
                "battery",
                "zwrt_bsp.battery",
                "list",
                Duration::from_secs(interval_s),
            )],
            Box::new(rec.clone()),
        );
        (hub, rec)
    }

    const SAMPLE: Duration = Duration::from_secs(1);

    #[test]
    fn block_never_read_is_stale_null() {
        let (hub, rec) = make_hub(5);
        let v = hub.view("battery").unwrap();
        assert_eq!(
            (v.stale, v.data.clone(), v.observed_at, v.revision),
            (true, Value::Null, 0, 0)
        );
        assert!(hub.legacy("battery").is_err());
        // 失败不翻转（本来就 stale），不发事件。
        hub.record_read(0, Err("x".into()), Instant::now());
        assert!(rec.blocks("battery").is_empty());
        // 心跳里 observed_at 是 0。
        let hb = hub.round_end(Instant::now(), SAMPLE);
        assert_eq!(hb.blocks, vec![("battery", 0)]);
        // 第一次读成功：revision 1（数据变化和 stale 翻转合计只 +1）。
        hub.record_read(0, Ok(json!({"battery_capacity":80})), Instant::now());
        let ev = rec.blocks("battery");
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].revision, ev[0].stale), (1, false));
        assert!(ev[0].observed_at > 0);
    }

    #[test]
    fn block_revision_unchanged_when_data_same() {
        let (hub, rec) = make_hub(5);
        let now = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), now);
        hub.record_read(0, Ok(json!({"a":1})), now);
        hub.record_read(0, Ok(json!({"a":1})), now);
        assert_eq!(hub.view("battery").unwrap().revision, 1);
        assert_eq!(rec.blocks("battery").len(), 1);
    }

    #[test]
    fn block_revision_bumps_on_data_change() {
        let (hub, rec) = make_hub(5);
        let now = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), now);
        hub.record_read(0, Ok(json!({"a":2})), now);
        let ev = rec.blocks("battery");
        assert_eq!(ev.iter().map(|e| e.revision).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(ev[1].data, json!({"a":2}));
        // seq 是全局计数，每条事件 +1。
        assert_eq!(ev[1].seq, ev[0].seq + 1);
    }

    #[test]
    fn block_any_change_publishes() {
        let (hub, rec) = make_hub(5);
        let now = Instant::now();
        hub.record_read(0, Ok(json!({"a":1,"b":{"c":[1,2]}})), now);
        hub.record_read(0, Ok(json!({"a":1,"b":{"c":[1,3]}})), now);
        hub.record_read(0, Ok(json!({"a":1,"b":{"c":[1,3]},"d":null})), now);
        assert_eq!(rec.blocks("battery").len(), 3);
        // 返回的不是对象：读失败（无效数据）。
        hub.record_read(0, Ok(json!([1])), now);
        assert!(rec.blocks("battery")[3].stale);
    }

    #[test]
    fn block_stale_flip_publishes_twice_same_data() {
        let (hub, rec) = make_hub(5);
        let now = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), now);
        let first_seen = rec.blocks("battery")[0].observed_at;
        hub.record_read(0, Err("timeout".into()), now);
        // 连续失败不再发。
        hub.record_read(0, Err("timeout".into()), now);
        hub.record_read(0, Ok(json!({"a":1})), now);
        let ev = rec.blocks("battery");
        assert_eq!(ev.len(), 3, "{ev:?}");
        assert_eq!(
            ev.iter().map(|e| (e.revision, e.stale)).collect::<Vec<_>>(),
            [(1, false), (2, true), (3, false)]
        );
        // stale 时 data 是保留的旧值，observed_at 不更新。
        assert_eq!(ev[1].data, json!({"a":1}));
        assert_eq!(ev[1].observed_at, first_seen);
        assert_eq!(ev[2].data, json!({"a":1}));
    }

    #[test]
    fn block_starved_to_max_age_goes_stale() {
        // 间隔 5 秒 → max_age = max(15, 15) = 15 秒。
        let (hub, rec) = make_hub(5);
        let t0 = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), t0);
        hub.round_end(t0 + Duration::from_secs(15), SAMPLE);
        assert!(!hub.view("battery").unwrap().stale);
        hub.round_end(t0 + Duration::from_millis(15_001), SAMPLE);
        let ev = rec.blocks("battery");
        assert_eq!(ev.len(), 2);
        assert!(ev[1].stale);
        assert_eq!(ev[1].data, json!({"a":1}));
        assert!(hub.legacy("battery").is_err());
        // 已经 stale，再过 max_age 不再翻转。
        hub.round_end(t0 + Duration::from_secs(100), SAMPLE);
        assert_eq!(rec.blocks("battery").len(), 2);

        // 间隔 60 秒 → max_age 180 秒；间隔 0 → 按采样间隔 ×3，最少 15 秒。
        let (hub, rec) = make_hub(60);
        hub.record_read(0, Ok(json!({"a":1})), t0);
        hub.round_end(t0 + Duration::from_secs(179), SAMPLE);
        assert_eq!(rec.blocks("battery").len(), 1);
        hub.round_end(t0 + Duration::from_secs(181), SAMPLE);
        assert_eq!(rec.blocks("battery").len(), 2);
        let (hub, rec) = make_hub(0);
        hub.record_read(0, Ok(json!({"a":1})), t0);
        hub.round_end(t0 + Duration::from_secs(16), Duration::from_secs(6));
        assert_eq!(
            rec.blocks("battery").len(),
            1,
            "6 s × 3 = 18 s，16 s 不置 stale"
        );
        hub.round_end(t0 + Duration::from_secs(16), Duration::from_secs(1));
        assert_eq!(rec.blocks("battery").len(), 2, "1 s × 3 < 15 s，按 15 s 算");
    }

    #[test]
    fn block_stale_clears_after_recovery_read() {
        let (hub, rec) = make_hub(5);
        let t0 = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), t0);
        hub.round_end(t0 + Duration::from_secs(20), SAMPLE);
        hub.record_read(0, Ok(json!({"a":1})), t0 + Duration::from_secs(21));
        let ev = rec.blocks("battery");
        assert_eq!(
            ev.iter().map(|e| (e.revision, e.stale)).collect::<Vec<_>>(),
            [(1, false), (2, true), (3, false)]
        );
        assert_eq!(hub.legacy("battery"), Ok(json!({"a":1})));
        // 恢复时值也变了：一条事件，带新值。
        hub.record_read(0, Err("x".into()), t0 + Duration::from_secs(22));
        hub.record_read(0, Ok(json!({"a":2})), t0 + Duration::from_secs(23));
        let ev = rec.blocks("battery");
        assert_eq!(ev.len(), 5);
        assert_eq!((ev[4].revision, ev[4].stale), (5, false));
        assert_eq!(ev[4].data, json!({"a":2}));
    }

    #[test]
    fn heartbeat_takes_seq_after_blocks() {
        let (hub, rec) = make_hub(5);
        let now = Instant::now();
        hub.record_read(0, Ok(json!({"a":1})), now);
        let hb = hub.round_end(now, SAMPLE);
        assert_eq!(hb.seq, 2);
        let seqs: Vec<u64> = rec
            .events()
            .iter()
            .map(|e| match e {
                Event::Block(b) => b.seq,
                Event::Heartbeat(h) => h.seq,
            })
            .collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(hub.snapshot().0, 2);
    }
}
