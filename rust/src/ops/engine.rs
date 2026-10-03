//! 事务引擎（write-op-layer.md 方案 B）：一把锁、覆盖与插队、确认驱动、落盘续跑、旧请求队列。
//!
//! - 同一时刻最多一个进行中的事务（含等确认的那段时间）；别的写回 busy（说清正在做什么、谁、多久）。
//! - 能插队的：同一项的用户写和 guard（覆盖，旧事务记 superseded，新事务继承它的退回目标，D35）；
//!   安全类写（关数据/漫游，旧事务记 preempted，D14）；对进行中事务的「立即退回」「保留现状」。
//! - 每一步对设备的读写都是执行者里的短任务（`Device` 经 `state::ubus`）；锁只在做决定时拿，
//!   从不跨 await 持有。
//! - 每次动设备之前先落意图、之后落结果（`pending.json`，D31）；datad 重启后接着确认，不重做。
//! - 没有 source 的旧请求：锁被占或执行者队列满时先回和今天一样的成功，排进旧请求队列，
//!   锁空出来再按原来的 `/control` 处理执行一次（每项只留最新、120 秒过期、安全类插队时清空）。

use super::{
    pending::Store,
    spec::{self, Spec},
    txn::{self, NewTxn, Phase, ProbeTarget, Reading, Reason, Source, Txn},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 引擎对设备的全部读写和时钟。真机是 `UbusDevice`，测试是假基带。
pub trait Device: Send + Sync + 'static {
    /// 一次新鲜读数（不用缓存）：这一项的配置读回、是否已注册、完整 SIM 身份、数据通路。
    fn read(&self, spec: &'static Spec) -> impl Future<Output = Result<Reading, String>> + Send;
    /// 一次 DNS 探测（D25、D33）：绑定到蜂窝接口，问运营商 DNS，有回答就算通。
    fn probe(&self, target: &ProbeTarget) -> impl Future<Output = Result<(), String>> + Send;
    fn write(
        &self,
        spec: &'static Spec,
        value: &str,
    ) -> impl Future<Output = Result<Value, WriteError>> + Send;
    /// BOOTTIME 毫秒：datad 重启不归零、休眠时也走。所有时限都按它算，tokio 的计时只当节拍。
    fn now_ms(&self) -> u64;
    fn boot_id(&self) -> String;
}

/// 写调用失败。`timed_out` = 我们这边超时了，原厂那边做没做不知道（D28）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    pub message: String,
    pub timed_out: bool,
}

/// 执行一个排队的旧请求（走原来的 `/control` 处理）；返回 false = 执行者队列满，过一会再试。
pub type LegacyRunner = Arc<dyn Fn(String, Value) -> BoxFuture<bool> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Config {
    /// 到点没通自动退回（D30：所有写入口收进 datad 并验收之前默认关）。
    pub rollback: bool,
    /// 确认时多久读一次。
    pub poll: Duration,
    /// 旧请求排队多久就丢掉。
    pub legacy_ttl_ms: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let ms = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        };
        Self {
            rollback: std::env::var("ZWRT_DATAD_ROLLBACK").is_ok_and(|v| v == "1"),
            poll: Duration::from_millis(ms("ZWRT_DATAD_OP_POLL_MS", 2_000)),
            legacy_ttl_ms: ms("ZWRT_DATAD_LEGACY_TTL_MS", 120_000),
        }
    }
}

/// 一次新写。
pub struct Request {
    pub spec: &'static Spec,
    pub target: String,
    pub source: Source,
    pub op_id: Option<String>,
    pub undo: bool,
}

#[derive(Debug)]
pub enum Submit {
    /// 这个 op_id 已经有了（幂等）：只回现有状态，不再动设备。
    Existing(Value),
    /// 别的事务进行中：它是什么。
    Busy(Value),
    /// 写之前读不到当前值，没动设备。
    NoCapture(String),
    /// 写了：写调用的结果（报错时事务仍按读回判断）和事务状态。
    Applied {
        result: Result<Value, String>,
        op: Value,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyGate {
    /// 照原来的处理做。
    Pass,
    /// 回成功、进旧请求队列。
    Queue,
}

struct LegacyReq {
    item: &'static str,
    action: String,
    params: Value,
    at_ms: u64,
}

const RECENT: usize = 16;
/// 等确认期间至少隔这么久落一次盘（重启后算上一次开机等了多久）。
const SEEN_SAVE_MS: u64 = 10_000;

#[derive(Default)]
struct St {
    active: Option<Box<Txn>>,
    recent: VecDeque<Txn>,
    legacy: VecDeque<LegacyReq>,
    saved_seen_ms: u64,
}

struct Inner<D> {
    dev: D,
    cfg: Config,
    store: Store,
    st: Mutex<St>,
    /// 叫醒驱动（用户点了退回）：退回只由驱动发，不会发两次。
    wake: Notify,
    runner: OnceLock<LegacyRunner>,
    draining: AtomicBool,
}

pub struct Engine<D: Device> {
    inner: Arc<Inner<D>>,
}

impl<D: Device> Clone for Engine<D> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

fn new_op_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// 客户端给的 op_id：1–64 个字母、数字、`.`、`_`、`-`。
pub fn valid_op_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 进行中的事务能不能被这个请求覆盖：同一项、来源可以覆盖（用户写或 guard，D15/D16），
/// 并且旧事务在等确认（写调用或退回调用还在途时不行）。
fn overridable(t: &Txn, req: &Request) -> bool {
    t.item == req.spec.item
        && req.source.may_override()
        && matches!(t.phase, Phase::Verifying | Phase::RollingBack)
        && t.intent.is_none()
}

impl St {
    fn live(&mut self, op_id: &str) -> Option<&mut Txn> {
        match &mut self.active {
            Some(t) if t.op_id == op_id => Some(t.as_mut()),
            _ => None,
        }
    }

    fn find(&self, op_id: &str, now: u64) -> Option<Value> {
        if let Some(t) = &self.active
            && t.op_id == op_id
        {
            return Some(t.status(now));
        }
        self.recent
            .iter()
            .rev()
            .find(|t| t.op_id == op_id)
            .map(|t| t.status(now))
    }

    fn remember(&mut self, t: Txn) {
        if self.recent.len() >= RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(t);
    }

    /// 把进行中的事务移进最近列表（它已经是终态）。不删落盘：调用方决定。
    fn retire(&mut self) {
        if let Some(t) = self.active.take() {
            eprintln!(
                "ops: {} {} from {:?} ended {:?}/{:?}",
                t.op_id, t.action, t.source, t.phase, t.reason
            );
            self.remember(*t);
        }
    }
}

/// busy 回复里的「正在做什么」。
fn doing(t: &Txn, now: u64) -> Value {
    json!({
        "op_id": t.op_id,
        "action": t.action,
        "source": t.source,
        "phase": t.phase,
        "age_ms": now.saturating_sub(t.created_ms),
    })
}

impl<D: Device> Engine<D> {
    pub fn new(dev: D, cfg: Config, store: Store) -> Self {
        Self {
            inner: Arc::new(Inner {
                dev,
                cfg,
                store,
                st: Mutex::new(St::default()),
                wake: Notify::new(),
                runner: OnceLock::new(),
                draining: AtomicBool::new(false),
            }),
        }
    }

    pub fn set_legacy_runner(&self, runner: LegacyRunner) {
        let _ = self.inner.runner.set(runner);
    }

    fn lock(&self) -> MutexGuard<'_, St> {
        self.inner.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.inner.dev.now_ms()
    }

    /// 落盘当前事务：阶段变了每次都落，只是又等了一拍的隔 10 秒落一次；到终态删掉。
    fn persist(&self, st: &mut St, force: bool) {
        let Some(t) = &st.active else {
            return;
        };
        if t.phase.is_final() {
            self.inner.store.clear();
        } else if force || t.seen_ms.saturating_sub(st.saved_seen_ms) >= SEEN_SAVE_MS {
            self.inner.store.save(t);
            st.saved_seen_ms = t.seen_ms;
        }
    }

    /// 当前事务到了终态：放锁、记进最近列表、删落盘。
    fn finish(&self, st: &mut St) {
        if st.active.as_ref().is_some_and(|t| t.phase.is_final()) {
            st.retire();
            self.inner.store.clear();
        }
    }

    /// datad 启动时：有落盘的事务就接着跑（D13、D18、D31）。没有就什么都不读（不多发 ubus）。
    /// 整个过程拿着跨进程写锁（D29）：应急直写正在写时，等它写完再看 takeover 标记。
    pub async fn start(&self) {
        let _lock = super::write_lock::acquire().await;
        let takeover = self.inner.store.take_takeover();
        let Some(mut t) = self.inner.store.load() else {
            if takeover {
                eprintln!("ops: takeover marker found, no change was pending");
            }
            return;
        };
        let now = self.now();
        if takeover {
            t.cancel(Reason::Takeover);
        } else {
            // 要退回的（reboot_loop）由驱动发：发之前先核对 SIM。
            if let txn::Next::Rollback(v) = t.resume(&self.inner.dev.boot_id(), now) {
                eprintln!(
                    "ops: {} still unconfirmed after a reboot, rolling back to {v}",
                    t.op_id
                );
            }
        }
        let op_id = t.op_id.clone();
        eprintln!(
            "ops: resuming {op_id} {} ({:?}, boots {})",
            t.action, t.phase, t.boots
        );
        let live = {
            let mut st = self.lock();
            st.active = Some(Box::new(t));
            self.persist(&mut st, true);
            self.finish(&mut st);
            st.active.is_some()
        };
        if live {
            self.spawn_driver(op_id);
        }
    }

    /// 一次新写（带 source 的客户端）。
    ///
    /// 先看一眼锁（明显 busy 就不读设备），再读当前值和 SIM，然后在一把锁里重新判断、
    /// 覆盖旧事务（superseded，继承退回目标，D35）、落盘新事务的写意图——中间没有空窗，
    /// 被覆盖事务的 pending.json 直接被新的盖掉。
    pub async fn submit(&self, req: Request) -> Submit {
        let now = self.now();
        let op_id = req.op_id.clone().unwrap_or_else(new_op_id);
        {
            let st = self.lock();
            if let Some(v) = st.find(&op_id, now) {
                return Submit::Existing(v);
            }
            if let Some(t) = &st.active
                && !overridable(t, &req)
            {
                return Submit::Busy(doing(t, now));
            }
        }

        // 写之前读当前值和 SIM（退回目标、R3 的「旧值」、D32 的 SIM 身份）。
        let (old, sim, conn) = match self.inner.dev.read(req.spec).await {
            Ok(Reading {
                value: Some(v),
                sim,
                data,
                ..
            }) => (v, sim, data.map(|d| d.conn).filter(|c| !c.ipv4.is_empty())),
            Ok(_) => {
                return Submit::NoCapture("cannot read current value: not reported".into());
            }
            Err(e) => return Submit::NoCapture(format!("cannot read current value: {e}")),
        };
        let now = self.now();
        {
            let mut st = self.lock();
            if let Some(v) = st.find(&op_id, now) {
                return Submit::Existing(v);
            }
            let mut inherit = None;
            if let Some(t) = st.active.as_mut() {
                if !overridable(t, &req) {
                    return Submit::Busy(doing(t, now));
                }
                inherit = Some(t.rollback_to.clone());
                t.cancel(Reason::Superseded);
                st.retire();
            }
            let mut t = Txn::new(
                NewTxn {
                    op_id: op_id.clone(),
                    action: req.spec.action.into(),
                    item: req.spec.item.into(),
                    source: req.source,
                    undo: req.undo,
                    target: req.target.clone(),
                    rollback_to: inherit.unwrap_or_else(|| old.clone()),
                    old,
                    sim,
                    conn,
                    confirm: req.spec.confirm,
                    // legacy 来源的写不带自动退回。
                    rollback_enabled: self.inner.cfg.rollback && req.source != Source::Legacy,
                    deadline_ms: req.spec.deadline_ms(),
                    boot_id: self.inner.dev.boot_id(),
                },
                now,
            );
            t.begin_apply();
            st.active = Some(Box::new(t));
            self.persist(&mut st, true);
        }
        let result = self.inner.dev.write(req.spec, &req.target).await;
        let now = self.now();
        let (op, live) = {
            let mut st = self.lock();
            let live = match st.live(&op_id) {
                Some(t) => {
                    t.applied(
                        match &result {
                            Ok(_) => txn::Applied::Done,
                            Err(e) if e.timed_out => txn::Applied::Unknown,
                            Err(_) => txn::Applied::Failed,
                        },
                        now,
                    );
                    true
                }
                // 写调用在途时被安全类写打断了（preempted）。
                None => false,
            };
            self.persist(&mut st, true);
            (st.find(&op_id, now).unwrap_or(Value::Null), live)
        };
        if live {
            self.spawn_driver(op_id);
        } else {
            self.kick_legacy();
        }
        Submit::Applied {
            result: result.map_err(|e| e.message),
            op,
        }
    }

    fn spawn_driver(&self, op_id: String) {
        let e = self.clone();
        tokio::spawn(async move { e.drive(op_id).await });
    }

    /// 等确认：要退回就先退回（只有这里发退回）；否则每拍读一次，喂给状态机。
    async fn drive(&self, op_id: String) {
        loop {
            let (generation, spec, rollback) = {
                let mut st = self.lock();
                let Some(t) = st.live(&op_id) else { break };
                let rollback = (t.phase == Phase::RollingBack
                    && t.intent == Some(txn::Intent::Rollback))
                .then(|| t.rollback_to.clone());
                (t.generation, spec::find(&t.action), rollback)
            };
            // 发之前读不到设备时 send_rollback 回 false：下一拍再试。
            if let Some(v) = rollback
                && self.send_rollback(&op_id, spec, v).await
            {
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(self.inner.cfg.poll) => {}
                _ = self.inner.wake.notified() => continue,
            }
            let mut reading = match spec {
                Some(s) => Some(self.inner.dev.read(s).await),
                None => None,
            };
            // 读数别的条件都齐了：在锁外做一次 DNS 探测（不经执行者、不拿写锁）。
            if let Some(Ok(r)) = &mut reading {
                let target = {
                    let mut st = self.lock();
                    let Some(t) = st.live(&op_id) else { break };
                    t.wants_probe(r, self.now())
                };
                if let Some(target) = target {
                    let result = self.inner.dev.probe(&target).await;
                    if let Err(e) = &result {
                        self.log_probe_failure(&op_id, &target, e);
                    }
                    r.probe = Some(result.is_ok());
                }
            }
            let now = self.now();
            let mut st = self.lock();
            let Some(t) = st.live(&op_id) else { break };
            if t.generation != generation {
                // 读的时候阶段变了（用户点了退回/保留）：这个读数不算。
                continue;
            }
            // 结果都在事务里：终态由 finish 收，要退回的下一圈由上面发。
            let _ = match &reading {
                Some(Ok(r)) => t.on_reading(r, now),
                _ => t.on_tick(now),
            };
            let changed = t.generation != generation;
            self.persist(&mut st, changed);
            self.finish(&mut st);
        }
        self.kick_legacy();
    }

    /// 探测失败每轮只在第一次失败、和失败满次数时各记一行（契约测试里每拍都会失败）。
    fn log_probe_failure(&self, op_id: &str, target: &ProbeTarget, e: &str) {
        let fails = {
            let mut st = self.lock();
            match st.live(op_id) {
                Some(t) => t.probe_fails,
                None => return,
            }
        };
        // 这次失败还没记进事务，fails 是之前的次数；已满 = 新的一轮或换了连接（会重新计数）。
        let first = fails == 0 || fails >= txn::PROBE_TRIES;
        if first || fails + 1 == txn::PROBE_TRIES {
            eprintln!(
                "ops: {op_id} DNS probe on {} failed ({e}){}",
                if target.iface.is_empty() {
                    "no interface"
                } else {
                    &target.iface
                },
                if fails + 1 == txn::PROBE_TRIES {
                    "; next round in 30 s"
                } else {
                    ""
                }
            );
        }
    }

    /// 发退回（意图已经在事务里）。发之前先读一次核对 SIM（D32）：换了卡就放弃、不写；
    /// 读不到就不发，返回 false（下一拍再试）。发出去以后只按读回判断，不发第二次。
    async fn send_rollback(&self, op_id: &str, spec: Option<&'static Spec>, value: String) -> bool {
        {
            let mut st = self.lock();
            if st.live(op_id).is_none() {
                return true;
            }
            self.persist(&mut st, true);
        }
        let Some(spec) = spec else {
            // 描述表里已经没有这个动作（旧版本落的盘）：发不了，按发过处理，到点记 rollback_failed。
            self.rollback_sent(op_id);
            return true;
        };
        let reading = self.inner.dev.read(spec).await;
        {
            let mut st = self.lock();
            let Some(t) = st.live(op_id) else { return true };
            let check = match &reading {
                Ok(r) => txn::sim_check(&t.sim, &r.sim),
                Err(_) => txn::SimCheck::Unknown,
            };
            match check {
                txn::SimCheck::Same => {}
                txn::SimCheck::Changed => {
                    t.cancel(Reason::SimChanged);
                    self.finish(&mut st);
                    return true;
                }
                txn::SimCheck::Unknown => return false,
            }
        }
        if let Err(e) = self.inner.dev.write(spec, &value).await {
            eprintln!("ops: {op_id} rollback write failed: {}", e.message);
        }
        self.rollback_sent(op_id);
        true
    }

    fn rollback_sent(&self, op_id: &str) {
        let now = self.now();
        let mut st = self.lock();
        if let Some(t) = st.live(op_id) {
            t.rollback_sent(now);
            self.persist(&mut st, true);
        }
    }

    /// `op.status`：指定 op_id 的，或当前的 / 最近结束的那个。
    pub fn status(&self, op_id: Option<&str>) -> Value {
        let now = self.now();
        let st = self.lock();
        match op_id {
            Some(id) => st.find(id, now).unwrap_or(Value::Null),
            None => match &st.active {
                Some(t) => t.status(now),
                None => st.recent.back().map_or(Value::Null, |t| t.status(now)),
            },
        }
    }

    /// 「立即退回」：记下意图、落盘，叫醒驱动去发（发之前核对 SIM）。
    pub fn revert(&self, op_id: &str) -> Result<Value, String> {
        let now = self.now();
        let mut st = self.lock();
        let Some(t) = st.live(op_id) else {
            return Err("no such change in progress".into());
        };
        t.revert()?;
        let v = t.status(now);
        self.persist(&mut st, true);
        drop(st);
        self.inner.wake.notify_one();
        Ok(v)
    }

    /// 「保留现状」。
    pub fn keep(&self, op_id: &str) -> Result<Value, String> {
        let now = self.now();
        let v = {
            let mut st = self.lock();
            let Some(t) = st.live(op_id) else {
                return Err("no such change in progress".into());
            };
            t.keep()?;
            self.finish(&mut st);
            st.find(op_id, now).unwrap_or(Value::Null)
        };
        self.kick_legacy();
        Ok(v)
    }

    /// 安全类写（关数据/漫游）马上插队：取消进行中的事务（preempted），清空旧请求队列。
    pub fn preempt(&self) {
        let mut st = self.lock();
        if let Some(t) = st.active.as_mut() {
            t.cancel(Reason::Preempted);
            self.finish(&mut st);
        }
        Self::drop_legacy(&mut st, "a safety write");
    }

    /// 重启/关机被接受：清空旧请求队列（进行中的事务不放弃，开机后接着确认）。
    pub fn clear_legacy(&self, why: &str) {
        Self::drop_legacy(&mut self.lock(), why);
    }

    fn drop_legacy(st: &mut St, why: &str) {
        for r in st.legacy.drain(..) {
            eprintln!("ops: dropped queued legacy {} because of {why}", r.action);
        }
    }

    /// 没有 source 的旧请求进来时：`item` 是它改的项（不在描述表里为 None），`safety` 是 D14 的安全类。
    pub fn legacy_gate(&self, item: Option<&'static str>, safety: bool) -> LegacyGate {
        if safety {
            self.preempt();
            return LegacyGate::Pass;
        }
        let Some(item) = item else {
            return LegacyGate::Pass;
        };
        let mut st = self.lock();
        let Some(t) = st.active.as_mut() else {
            return LegacyGate::Pass;
        };
        if t.item == item
            && matches!(t.phase, Phase::Verifying | Phase::RollingBack)
            && t.intent.is_none()
        {
            // 同一项：当作覆盖写，旧事务记 superseded。
            t.cancel(Reason::Superseded);
            self.finish(&mut st);
            return LegacyGate::Pass;
        }
        LegacyGate::Queue
    }

    /// 排一个旧请求（每项只留最新的）。
    pub fn enqueue_legacy(&self, item: &'static str, action: &str, params: Value) {
        let now = self.now();
        {
            let mut st = self.lock();
            st.legacy.retain(|r| {
                let keep = r.item != item;
                if !keep {
                    eprintln!("ops: queued legacy {} replaced by a newer one", r.action);
                }
                keep
            });
            st.legacy.push_back(LegacyReq {
                item,
                action: action.into(),
                params,
                at_ms: now,
            });
        }
        self.kick_legacy();
    }

    /// 锁空着就把排队的旧请求一个个做掉；执行者队列满就隔 1 秒再试；排太久的丢掉。
    fn kick_legacy(&self) {
        let Some(runner) = self.inner.runner.get().cloned() else {
            return;
        };
        if self.inner.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        let e = self.clone();
        tokio::spawn(async move {
            loop {
                let req = {
                    let now = e.now();
                    let ttl = e.inner.cfg.legacy_ttl_ms;
                    let mut st = e.lock();
                    if st.active.is_some() {
                        None
                    } else {
                        st.legacy.retain(|r| {
                            let fresh = now.saturating_sub(r.at_ms) < ttl;
                            if !fresh {
                                eprintln!("ops: dropped legacy {} after {ttl} ms", r.action);
                            }
                            fresh
                        });
                        st.legacy.pop_front()
                    }
                };
                let Some(req) = req else {
                    e.inner.draining.store(false, Ordering::Release);
                    // 放下标志之后又来了活：再抢一次。
                    let more = {
                        let st = e.lock();
                        st.active.is_none() && !st.legacy.is_empty()
                    };
                    if more && !e.inner.draining.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    return;
                };
                if !runner(req.action.clone(), req.params.clone()).await {
                    // 执行者队列满：放回队头（这期间同一项来了更新的就不放回）。
                    {
                        let mut st = e.lock();
                        if !st.legacy.iter().any(|r| r.item == req.item) {
                            st.legacy.push_front(req);
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests;
