//! 单一采集执行者（docs/STATE_V2.md 第 7、8 节，R13/R3/R12/R2）。
//!
//! 一个 tokio 任务持有 ubus 后端，datad 里所有 ubus 请求都经过它，同一时间最多一个在途：
//!
//! - **采集轮**：先读到期的块（`block::Hub`，每轮 ubus 预算 3 秒、没轮到的下一轮先读），
//!   再跑还没迁移的旧采集（`RoundDriver::legacy`，即 `state::collect` 那一套，不受预算截断），
//!   最后 `Hub::round_end`：max_age、轮末钩子、心跳（`seq` 在那里分配）。
//! - **任务**：`/control` 整个处理过程作为一个控制任务交给执行者（`Executor::control`，排队上限 8，
//!   满了立即 `Busy`）；其他地方（登录校验等）在执行者外调 `state::ubus` 时，自动包成一个内部任务
//!   （不计上限）。任务在块与块之间、旧采集的 `ubus_ttl` 调用之前（`preempt`）、以及轮间睡眠时执行，
//!   先来先做，一个任务里的多次调用连着做完。
//!
//! 执行者里的代码用 task-local（`CTX`）认出自己，直接用后端；执行者外的调用走队列。
//! 任务只在这些「安全点」执行：那里调用方不持有任何锁（旧采集里 `sms::prepare` 持着锁调 ubus，
//! 所以 `state::ubus` 本身不是安全点，只有 `ubus_ttl` 是）。

use crate::{
    block::{Hub, Schedule},
    ubus::{
        backend::UbusBackend,
        client::{RoundSkips, UbusError},
    },
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    time::Instant,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// V2-19：每轮 ubus 预算。
pub const ROUND_BUDGET: Duration = Duration::from_secs(3);
/// V2-25：排队中的控制任务上限。
pub const CONTROL_QUEUE: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub budget: Duration,
    /// `ZWRT_DATAD_CACHE` 没设成 0。
    pub cache: bool,
    pub control_queue: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            budget: ROUND_BUDGET,
            cache: true,
            control_queue: CONTROL_QUEUE,
        }
    }
}

/// 控制队列满了（V2-25）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// 可装箱的后端（`UbusBackend` 用 `impl Future`，不能直接做 trait object）。
trait DynBackend: Send {
    fn call<'a>(
        &'a mut self,
        object: &'a str,
        method: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, UbusError>>;
}

impl<B: UbusBackend> DynBackend for B {
    fn call<'a>(
        &'a mut self,
        object: &'a str,
        method: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, UbusError>> {
        Box::pin(UbusBackend::call(self, object, method, args))
    }
}

/// 旧采集（未迁移的读取）。每轮在块之后跑一次，future 在执行者里执行。
pub trait RoundDriver: Send + Sync {
    fn legacy(&self) -> BoxFuture<'static, ()>;
}

struct Job {
    control: bool,
    fut: BoxFuture<'static, ()>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// 排队中的控制任务数（正在执行的不算）。
    controls: usize,
}

#[derive(Default)]
struct RoundState {
    skips: RoundSkips,
    used: Duration,
}

/// 计数（测试和日志用）。
#[derive(Debug, Default)]
pub struct Stats {
    pub rounds: AtomicU64,
    pub calls: AtomicU64,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Notify,
    backend: tokio::sync::Mutex<Box<dyn DynBackend>>,
    round: Mutex<RoundState>,
    hub: Arc<Hub>,
    cfg: Config,
    interval_ms: AtomicU64,
    /// 轮转：下一轮从这块开始（V2-20）。
    cursor: Mutex<usize>,
    driver: Mutex<Option<Arc<dyn RoundDriver>>>,
    /// 下一轮不等采样间隔、立即开始（短信事件，V2-31）。
    kick: AtomicBool,
    stats: Stats,
}

#[derive(Clone)]
struct Ctx {
    shared: Arc<Shared>,
    /// 采集轮里（计预算、按 RoundSkips 跳过；可以在安全点插任务）。
    round: bool,
}

tokio::task_local! {
    static CTX: Ctx;
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct Executor {
    shared: Arc<Shared>,
}

static GLOBAL: OnceLock<Executor> = OnceLock::new();

/// 装成全局执行者（`state::ubus` 在执行者外调用时用它）。只装一次。
pub fn install(e: &Executor) {
    let _ = GLOBAL.set(e.clone());
}

/// 当前是否在执行者里（测试用）。
#[cfg(test)]
pub fn on_executor() -> bool {
    CTX.try_with(|_| ()).is_ok()
}

/// 发一个 ubus 请求。执行者里直接发；执行者外包成内部任务排队。
pub async fn call(object: &str, method: &str, args: &Value) -> Result<Value, UbusError> {
    if let Ok(ctx) = CTX.try_with(Ctx::clone) {
        return ctx.shared.call_here(ctx.round, object, method, args).await;
    }
    let Some(exec) = GLOBAL.get() else {
        return Err(UbusError::Io("ubus executor not running".into()));
    };
    exec.call(object, method, args).await
}

/// 安全点：在采集轮里时，先把排队的任务做完（V2-24）。调用方不能持有任何锁。
pub async fn preempt() {
    if let Ok(ctx) = CTX.try_with(Ctx::clone)
        && ctx.round
    {
        ctx.shared.drain().await;
    }
}

impl Shared {
    async fn call_here(
        &self,
        round: bool,
        object: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, UbusError> {
        if round && lock(&self.round).skips.is_skipped(object) {
            return Err(UbusError::Skipped {
                object: object.into(),
            });
        }
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();
        let r = self.backend.lock().await.call(object, method, args).await;
        if round {
            let mut rs = lock(&self.round);
            rs.used += start.elapsed();
            if let Err(e) = &r {
                rs.skips.record(e);
            }
        }
        r
    }

    /// 把排队的任务按先后做完（包括做的过程中新来的）。
    async fn drain(self: &Arc<Self>) {
        loop {
            let job = {
                let mut q = lock(&self.queue);
                let job = q.jobs.pop_front();
                if job.as_ref().is_some_and(|j| j.control) {
                    q.controls -= 1;
                }
                job
            };
            let Some(job) = job else { break };
            let ctx = Ctx {
                shared: self.clone(),
                round: false,
            };
            CTX.scope(ctx, job.fut).await;
        }
    }

    fn sample_interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.load(Ordering::Relaxed))
    }

    fn schedule(&self) -> Schedule {
        Schedule {
            cache: self.cfg.cache,
            sample_interval: self.sample_interval(),
        }
    }

    /// 一轮：块（预算内、轮转）→ 旧采集 → 轮末（max_age、心跳）。
    async fn round<T: Send>(self: &Arc<Self>, legacy: Option<BoxFuture<'_, T>>) -> Option<T> {
        {
            let mut rs = lock(&self.round);
            rs.skips.clear();
            rs.used = Duration::ZERO;
        }
        let ctx = Ctx {
            shared: self.clone(),
            round: true,
        };
        let out = CTX
            .scope(ctx, async {
                self.read_blocks().await;
                match legacy {
                    Some(f) => {
                        preempt().await;
                        Some(f.await)
                    }
                    None => None,
                }
            })
            .await;
        self.drain().await;
        self.hub.round_end(Instant::now(), self.sample_interval());
        self.stats.rounds.fetch_add(1, Ordering::Relaxed);
        out
    }

    async fn read_blocks(self: &Arc<Self>) {
        let n = self.hub.len();
        if n == 0 {
            return;
        }
        let start = *lock(&self.cursor) % n;
        for k in 0..n {
            let i = (start + k) % n;
            // V2-24：每读完一块（和读第一块之前），先做排队的任务。
            self.drain().await;
            if !self.hub.due(i, Instant::now(), self.schedule()) {
                continue;
            }
            // V2-19：满预算就不再开始新请求；没轮到的下一轮从这里开始（V2-20）。
            if lock(&self.round).used >= self.cfg.budget {
                *lock(&self.cursor) = i;
                return;
            }
            let (object, method, args) = self.hub.request(i);
            let r = self.call_here(true, object, method, &args).await;
            self.hub
                .record_read(i, r.map_err(|e| e.to_string()), Instant::now());
        }
    }

    fn push(&self, job: Job) -> Result<(), Busy> {
        {
            let mut q = lock(&self.queue);
            if job.control {
                if q.controls >= self.cfg.control_queue {
                    return Err(Busy);
                }
                q.controls += 1;
            }
            q.jobs.push_back(job);
        }
        self.wake.notify_one();
        Ok(())
    }

    /// 执行者主循环：没开始采集时只做任务；开始后「睡一个采样间隔（期间照做任务）→ 一轮」。
    async fn run(self: Arc<Self>) {
        let mut last_round_end = Instant::now();
        loop {
            let driver = lock(&self.driver).clone();
            let deadline = driver
                .as_ref()
                .map(|_| last_round_end + self.sample_interval());
            // V2-31：被踢过就不等间隔。轮本身仍在这个循环里串行跑，轮里照样先做排队的控制任务。
            let kicked = deadline.is_some() && self.kick.swap(false, Ordering::AcqRel);
            match deadline {
                Some(d) if kicked || Instant::now() >= d => {
                    let legacy = driver.map(|d| d.legacy());
                    self.round(legacy).await;
                    last_round_end = Instant::now();
                    continue;
                }
                Some(d) => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep_until(d) => {}
                    }
                }
                None => self.wake.notified().await,
            }
            self.drain().await;
            if deadline.is_none() && lock(&self.driver).is_some() {
                // 刚开始采集：从现在起睡一个间隔。
                last_round_end = Instant::now();
            }
        }
    }
}

impl Executor {
    /// 起执行者任务。之后要 `start_rounds` 才开始周期采集。
    pub fn spawn<B: UbusBackend + 'static>(
        backend: B,
        hub: Arc<Hub>,
        cfg: Config,
        sample_interval: Duration,
    ) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            wake: Notify::new(),
            backend: tokio::sync::Mutex::new(Box::new(backend)),
            round: Mutex::default(),
            hub,
            cfg,
            interval_ms: AtomicU64::new(sample_interval.as_millis() as u64),
            cursor: Mutex::new(0),
            driver: Mutex::new(None),
            kick: AtomicBool::new(false),
            stats: Stats::default(),
        });
        tokio::spawn(shared.clone().run());
        Self { shared }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.shared.hub
    }

    #[cfg(test)]
    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    pub fn interval_ms(&self) -> u64 {
        self.shared.interval_ms.load(Ordering::Relaxed)
    }

    /// `state.set_interval`：改执行者的睡眠间隔，正在睡的这一次按新间隔重算。
    pub fn set_interval_ms(&self, ms: u64) {
        self.shared.interval_ms.store(ms, Ordering::Relaxed);
        self.shared.wake.notify_one();
    }

    /// 从现在起周期采集：睡一个采样间隔，采一轮，如此往复。
    pub fn start_rounds(&self, driver: Arc<dyn RoundDriver>) {
        *lock(&self.shared.driver) = Some(driver);
        self.shared.wake.notify_one();
    }

    /// 立刻采一轮（启动时的第一轮），`legacy` 是这一轮的旧采集；返回它的结果。
    pub async fn round_now<T, F>(&self, legacy: F) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let shared = self.shared.clone();
        self.task(async move {
            shared
                .round(Some(Box::pin(legacy)))
                .await
                .expect("legacy result")
        })
        .await
    }

    fn submit<T, F>(&self, control: bool, fut: F) -> Result<oneshot::Receiver<T>, Busy>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.shared.push(Job {
            control,
            fut: Box::pin(async move {
                // 请求方断开了也做完（/control 一直是做完才回复）。
                let _ = tx.send(fut.await);
            }),
        })?;
        Ok(rx)
    }

    /// 控制任务（V2-24、V2-25）：排队中的已有 8 个就立即 `Busy`；否则挂到做完。
    pub async fn control<T, F>(&self, fut: F) -> Result<T, Busy>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let rx = self.submit(true, fut)?;
        Ok(rx.await.expect("executor dropped a control task"))
    }

    /// 发一个 ubus 请求：执行者里直接发，执行者外包成内部任务排队（不计控制队列上限）。
    pub async fn call(&self, object: &str, method: &str, args: &Value) -> Result<Value, UbusError> {
        if let Ok(ctx) = CTX.try_with(Ctx::clone) {
            return ctx.shared.call_here(ctx.round, object, method, args).await;
        }
        let (o, m, a) = (object.to_owned(), method.to_owned(), args.clone());
        self.task(async move {
            let ctx = CTX.with(Ctx::clone);
            ctx.shared.call_here(false, &o, &m, &a).await
        })
        .await
    }

    /// 内部任务（不计上限）。
    pub async fn task<T, F>(&self, fut: F) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let rx = self
            .submit(false, fut)
            .expect("internal tasks are unbounded");
        rx.await.expect("executor dropped a task")
    }

    /// 短信事件（V2-31）：`names` 的块标成立即读，并让下一轮不等采样间隔立即开始。
    /// 正在跑一轮时，这一轮跑完马上再跑一轮；多次调用在开始前只算一次。不另起并发的采集。
    pub fn kick(&self, names: &[&str]) {
        self.shared.hub.mark_immediate(Some(names));
        self.shared.kick.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }

    /// `/control` 成功后：相关块下一轮立即读，`None` = 全部块（R12）。不另起一轮（V2-27）。
    pub fn mark_immediate(&self, names: Option<&[&str]>) {
        self.shared.hub.mark_immediate(names);
    }
}

#[cfg(test)]
mod tests;
