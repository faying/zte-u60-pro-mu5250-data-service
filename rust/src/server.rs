use crate::{
    auth::{self, Sessions},
    block::{self, Hub},
    executor::{self, Executor, RoundDriver},
    model::{DatadVersion, Snapshot},
    state, v2,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::rejection::JsonRejection,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};
use std::{future::IntoFuture, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore, watch},
};
use tower_http::limit::RequestBodyLimitLayer;

mod write;
#[cfg(test)]
pub(crate) use write::control_busy;

#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}
struct Inner {
    snapshot: RwLock<Snapshot>,
    tx: watch::Sender<Snapshot>,
    /// 单一采集执行者：采样循环、全部 ubus 调用、/control 排队（STATE_V2.md 第 7、8 节）。
    exec: Executor,
    _data_dir: PathBuf,
    token: Option<String>,
    sessions: Mutex<Sessions>,
    /// `/v2/events` 的 SSE 连接名额。
    sse_slots: Arc<Semaphore>,
    /// `/v2` 的流（epoch + broadcast），事件由 `Hub` 在锁里发进来。
    feed: Arc<v2::Feed>,
    /// E4 写操作层：事务引擎（`ops/`）。
    ops: crate::ops::Engine<crate::ops::UbusDevice>,
    /// 启动进度（`App::start`）：监听先起来，第一轮采集和事务恢复在后台做。
    stage: watch::Sender<Stage>,
    /// 采样间隔（`App::start` 用）。
    interval: Duration,
    /// 收到 SIGTERM/SIGINT 后置 true（P2-4）：SSE 流看到就结束，服务在 `SHUTDOWN_GRACE` 内退出。
    stop: watch::Sender<bool>,
}

/// 收到退出信号后最多等连接收尾这么久（P2-4）。procd 的 `term_timeout` 约 5 秒，到了就 SIGKILL；
/// 这里留出余量。读不动的客户端（发送缓冲满）收不到流的结尾，graceful shutdown 会一直等它，所以要有上限。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// 启动进度（P1-3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    /// 第一轮还没采完：读数据的接口先等（`starting_gate`），`/healthz` 回 503。
    Starting,
    /// 有第一份快照了：读接口照常；`/control` 还在等事务恢复（`ops.start`）。
    Snapshot,
    /// 全部就绪。
    Ready,
}

/// 启动时 `/control` 最多等这么久（等第一轮和事务恢复做完），还没好就回 503 `busy`（客户端照队列满重试）。
const CONTROL_START_WAIT: Duration = Duration::from_secs(20);
/// 启动时读数据的请求最多等这么久（等第一轮采完），还没好就回 503 `starting`。
/// 先等而不是马上回 503：以前端口要到第一轮采完才监听，有的客户端把「连得上」当成「有快照」。
const READ_START_WAIT: Duration = Duration::from_secs(10);
/// `/healthz` 判卡住的线：执行者这么久没前进就回 503（和订阅方判卡死的 20 秒一样，V2-32）。
const HEALTH_STALL_MS: u64 = 20_000;

/// V2-22：距上一条心跳满这么久，独立定时器就补一条。
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// D18：应急直写脚本据此判断 datad 在不在（pid + /proc/<pid>/comm 前缀）。
/// `ZWRT_DATAD_PID_FILE`，默认 `/var/run/zwrt-datad.pid`，空串 = 不写。
pub fn write_pid_file() {
    let path = match std::env::var("ZWRT_DATAD_PID_FILE") {
        Ok(v) if v.is_empty() => return,
        Ok(v) => PathBuf::from(v),
        Err(_) => PathBuf::from("/var/run/zwrt-datad.pid"),
    };
    let tmp = path.with_extension("pid.tmp");
    let r = std::fs::write(&tmp, format!("{}\n", std::process::id()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = r {
        eprintln!("cannot write pid file {}: {e}", path.display());
    }
}

impl App {
    pub async fn new(data_dir: PathBuf, interval: Duration, token: Option<String>) -> Result<Self> {
        let feed = v2::Feed::new(v2::CAPACITY);
        let hub = Arc::new(Hub::new(
            block::phase1_blocks(),
            Box::new(v2::FeedSink(feed.clone())),
        ));
        let cfg = executor::Config {
            cache: state::cache_enabled(),
            cooldown: executor::Config::cooldown_from(
                std::env::var(executor::ENV_COOLDOWN_MS).ok().as_deref(),
            ),
            ..executor::Config::default()
        };
        let exec = Executor::spawn(
            crate::ubus::backend::Backend::from_env(),
            hub.clone(),
            cfg,
            interval,
        );
        executor::install(&exec);
        // V2-22：独立定时器补心跳（带 exec_age_ms）；V2-32：看门狗。第一轮之前就起：第一轮里每次调用都记前进，
        // 执行者闲着时不算卡（P1-3：以前第一轮在看门狗起来之前跑，ubusd 不回时没人管）。
        exec.start_heartbeat(HEARTBEAT_EVERY);
        if let Some(limit) = crate::watchdog::limit_from_env() {
            crate::watchdog::spawn(exec.clone(), limit);
        }
        // 第一轮之前的占位快照：只在 `Stage::Starting` 时存在，读接口这时先等第一轮（`starting_gate`），不会发出去。
        let placeholder = Snapshot {
            ts: 0,
            datad: Default::default(),
            fields: Map::new(),
        };
        let (tx, _) = watch::channel(placeholder.clone());
        let ops = crate::ops::Engine::new(
            crate::ops::UbusDevice::new(exec.clone()),
            crate::ops::Config::from_env(),
            crate::ops::pending::Store::open(crate::ops::ops_dir()),
            crate::ops::record::Record::open(crate::ops::ops_dir()),
        );
        let (stage, _) = watch::channel(Stage::Starting);
        Ok(Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(placeholder),
                tx,
                exec,
                _data_dir: data_dir,
                token,
                sessions: Mutex::new(Sessions::default()),
                sse_slots: Arc::new(Semaphore::new(16)),
                feed,
                ops,
                stage,
                interval,
                stop: watch::channel(false).0,
            }),
        })
    }

    /// 第一轮采集、周期采集、事务恢复、短信监听（P1-3：在监听起来之后做，`/healthz` 这期间回 503 `starting`）。
    /// 只调一次。
    pub async fn start(&self) {
        let app = self;
        // 第一轮在执行者里立即采（块 + 旧采集），之后执行者自己「睡一个采样间隔 → 一轮」。
        let interval_ms = app.inner.interval.as_millis() as u64;
        let hub = app.inner.exec.hub().clone();
        let initial = app
            .inner
            .exec
            .round_now(async move { state::collect(interval_ms, &hub).await })
            .await;
        *app.inner.snapshot.write().await = initial.clone();
        app.inner.tx.send_replace(initial);
        app.inner.stage.send_replace(Stage::Snapshot);
        app.inner.exec.start_rounds(Arc::new(app.clone()));
        // 排队的旧请求锁空出来后照原来的 /control 处理执行（返回 false = 执行者队列满）。
        let runner_app = app.clone();
        app.inner
            .ops
            .set_legacy_runner(Arc::new(move |action, params| {
                let app = runner_app.clone();
                Box::pin(async move {
                    let body = json!({"action": action, "params": params});
                    write::run_control(app, action, body).await.is_ok()
                })
            }));
        // 有落盘的事务就接着确认（没有就不读设备）。
        app.inner.ops.start().await;
        // V2-34：引擎每次状态变化都交 op 块（在引擎的锁里），接上时先交一次。
        let hub_exec = app.inner.exec.clone();
        app.inner.ops.set_observer(Arc::new(move |v| {
            hub_exec
                .hub()
                .record("op", Ok(v), tokio::time::Instant::now());
        }));
        // V2-31：监听短信事件，收到后短信读取立即重读、执行者立即开一轮（只订阅，不发请求）。
        let exec = app.inner.exec.clone();
        crate::ubus::listen::spawn_if_enabled(
            std::env::var(crate::ubus::listen::ENV_ENABLE)
                .ok()
                .as_deref(),
            crate::ubus::listen::Options::from_env(),
            Arc::new(move || {
                state::invalidate_sms_cache();
                exec.kick(&["sms"]);
            }),
        );
        app.inner.stage.send_replace(Stage::Ready);
    }

    fn stage(&self) -> Stage {
        *self.inner.stage.borrow()
    }

    /// 等到至少 `stage`，最多 `limit`；到了回 true。
    async fn wait_stage(&self, stage: Stage, limit: Duration) -> bool {
        let mut rx = self.inner.stage.subscribe();
        tokio::time::timeout(limit, rx.wait_for(|s| *s >= stage))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.read().await.clone()
    }
    /// 一轮里的旧采集（在执行者里跑，块已经读完）。别在持有 snapshot 锁时调 ubus：
    /// 控制任务只在 `ubus_ttl` 这类安全点插进来，这里的锁段里没有安全点。
    async fn refresh_snapshot(&self) {
        let next = state::collect(self.inner.exec.interval_ms(), self.inner.exec.hub()).await;
        // V2-34：op 块每轮交一次（进行中时倒计时跟着走）。
        self.inner.ops.publish_now();
        let mut old = self.inner.snapshot.write().await;
        let mut comparable = next.clone();
        comparable.ts = old.ts;
        if comparable != *old {
            *old = next.clone();
            let _ = self.inner.tx.send(next);
        } else {
            old.ts = next.ts;
        }
    }
}

impl RoundDriver for App {
    fn legacy(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let app = self.clone();
        Box::pin(async move { app.refresh_snapshot().await })
    }
}

impl App {
    /// 收到退出信号时完成（`stop` 置 true；`App` 已经没了也算）。SSE 流用它结束自己。
    fn stopped(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.inner.stop.subscribe();
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    }

    /// 在 `listener` 上提供服务（P1-3：监听在 `App::start` 之前就起来，启动中的请求见 `starting_gate`）。
    pub async fn serve(
        self,
        listener: TcpListener,
        require_auth: bool,
        open_auth_routes: bool,
    ) -> Result<()> {
        let mut router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(gone))
            .route("/events", get(gone))
            .route("/v2/events", get(v2_events))
            .route("/v2/state", get(v2_state))
            .route("/v2/screen", get(v2_screen))
            .route("/capabilities", get(capabilities))
            .route("/control", post(control))
            .route("/debug/legacy-hits", get(legacy_hits));
        if open_auth_routes {
            router = router
                .route("/auth/login", post(auth_login))
                .route("/auth/exchange", post(auth_exchange));
        }
        router = router.layer(middleware::from_fn_with_state(self.clone(), starting_gate));
        if require_auth {
            router = router.layer(middleware::from_fn_with_state(self.clone(), authenticate));
        }
        // 最外层：没登录、还在启动被 503 的旧请求也要数到。
        router = router.layer(middleware::from_fn(count_legacy));
        let router = router
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let server = axum::serve(
            crate::conn::DatadListener::new(listener),
            router.into_make_service_with_connect_info::<crate::conn::Peer>(),
        )
        .with_graceful_shutdown(shutdown(self.clone()))
        .into_future();
        // 从收到信号算起最多 `SHUTDOWN_GRACE`：还有连接没收尾（对端不读）也不再等。
        let deadline = {
            let stopped = self.stopped();
            async move {
                stopped.await;
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            }
        };
        let result = tokio::select! {
            result = server => result,
            () = deadline => {
                eprintln!("shutdown: connections still open after {SHUTDOWN_GRACE:?}, exiting anyway");
                Ok(())
            }
        };
        result?;
        Ok(())
    }
}

/// P1-3：第一轮采完之前（`Stage::Starting`），读数据的接口先等第一轮（最多 10 秒），还没好就回 503 `starting`
/// （客户端按连不上处理、过一会儿重试）；`/`、`/healthz`、`/version`、`/capabilities`、登录照常。
/// `/control` 在处理函数里等就绪（`wait_stage(Ready)`）。
async fn starting_gate(State(app): State<App>, request: Request, next: Next) -> Response {
    let open = matches!(
        request.uri().path(),
        "/" | "/healthz"
            | "/version"
            | "/capabilities"
            | "/control"
            | "/debug/legacy-hits"
            | "/state"
            | "/events"
            | "/auth/login"
            | "/auth/exchange"
    );
    if open || app.wait_stage(Stage::Snapshot, READ_START_WAIT).await {
        return next.run(request).await;
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"ok":false,"error":{"code":"starting","message":"datad is starting (first collection round not finished)"}})),
    )
        .into_response()
}

/// 旧接口访问计数（legacy_hits.rs）：`/state`、`/events` 在这里数；`/control` 要先解出动作，
/// 在处理函数里数，这里只把对端地址放进请求扩展。
async fn count_legacy(mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<crate::conn::Peer>>()
        .map(|info| info.0.0);
    if let path @ ("/state" | "/events") = request.uri().path() {
        crate::legacy_hits::hit(path, peer).await;
    }
    request.extensions_mut().insert(LegacyPeer(peer));
    next.run(request).await
}

#[derive(Clone, Copy)]
struct LegacyPeer(Option<std::net::SocketAddr>);

async fn legacy_hits() -> Json<Value> {
    Json(crate::legacy_hits::report())
}

async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if matches!(path, "/" | "/healthz" | "/auth/login" | "/auth/exchange") {
        return next.run(request).await;
    }
    let headers = request.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let legacy = headers.get("x-auth-token").and_then(|v| v.to_str().ok());
    let query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|part| part.strip_prefix("access_token="))
    });
    let presented = [bearer, legacy, query].into_iter().flatten().next();
    let static_valid = static_token_valid(app.inner.token.as_deref(), presented);
    let session_valid = if static_valid {
        false
    } else if let Some(value) = presented {
        app.inner.sessions.lock().await.validate(value)
    } else {
        false
    };
    if static_valid || session_valid {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":{"code":"unauthorized","message":"authentication required"}}))).into_response()
    }
}

fn static_token_valid(configured: Option<&str>, presented: Option<&str>) -> bool {
    configured.is_some_and(|wanted| {
        presented.is_some_and(|value| constant_time_eq(value.as_bytes(), wanted.as_bytes()))
    })
}

async fn auth_login(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some((username, password)) = basic_credentials(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_credentials"})),
        )
            .into_response();
    };
    if !auth::verify_password(&username, &password).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_credentials"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

async fn auth_exchange(
    State(app): State<App>,
    ConnectInfo(crate::conn::Peer(peer)): ConnectInfo<crate::conn::Peer>,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get("x-web-token")
        .or_else(|| headers.get("x-zte-webtoken"))
        .and_then(|value| value.to_str().ok());
    let Some(token) = token.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_webtoken"})),
        )
            .into_response();
    };
    let mode = headers
        .get("x-z-mode")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let tag = headers
        .get("x-z-tag")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("zwrt-datad");
    if !auth::verify_webtoken(token, mode, &peer.ip().to_string(), tag).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_webtoken"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    if encoded.len() > 1024 {
        return None;
    }
    let decoded = decode_base64(encoded)?;
    if decoded.len() > 512 || decoded.contains(&0) {
        return None;
    }
    let separator = decoded.iter().position(|b| *b == b':')?;
    if separator == 0 {
        return None;
    }
    let username = String::from_utf8(decoded[..separator].to_vec()).ok()?;
    let password = String::from_utf8(decoded[separator + 1..].to_vec()).ok()?;
    if username.len() >= 257 || password.len() >= 257 {
        return None;
    }
    Some((username, password))
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0u8;
    let mut out = Vec::new();
    for byte in value.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= if bits == 0 { 0 } else { (1 << bits) - 1 };
        }
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn index() -> &'static str {
    "zwrt-datad Rust rewrite\n"
}
/// `/healthz`（P1-3）：200 = 第一轮采完了、执行者没卡（`exec_age_ms` ≤ 20 秒）；503 = 还在启动（`starting`）
/// 或执行者卡住（`stalled`，订阅方也按 20 秒判卡死，V2-32）。不经执行者，卡住时照样回。
async fn health(State(app): State<App>) -> Response {
    let age = app.inner.exec.exec_age_ms();
    let status = if app.stage() < Stage::Snapshot {
        "starting"
    } else if age > HEALTH_STALL_MS {
        "stalled"
    } else {
        "ok"
    };
    let code = if status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({"ok": status == "ok", "status": status, "exec_age_ms": age})),
    )
        .into_response()
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
/// 旧的 `/state`、`/events`（2026-10 删，legacy-api-removal.md）：回 410，访问照样记进 legacy-hits，
/// 漏改的调用者看计数和这条错误就知道该换 `/v2/state`、`/v2/events`。
async fn gone() -> Response {
    (
        StatusCode::GONE,
        Json(json!({"ok":false,"error":{"code":"gone","message":"removed; use /v2/state and /v2/events"}})),
    )
        .into_response()
}
fn capability_controls() -> Vec<&'static str> {
    let mut controls = vec!["state.set_interval"];
    controls.extend_from_slice(crate::control::ACTIONS);
    controls
}

async fn capabilities() -> Json<Value> {
    let controls = capability_controls();
    Json(json!({
        "schema_version":1,
        "protocol":1,
        "events":["snapshot","block"],
        "transport":["http","sse"],
        "control":controls,
        "controls":controls,
        "rewrite":"rust"
    }))
}

fn sse_limit() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"ok":false,"error":{"code":"sse_client_limit","message":"too many SSE clients"}})),
    )
        .into_response()
}

/// `/v2/events`（STATE_V2.md 第 2–4 节）：SSE 连接名额满了回 503。
async fn v2_events(State(app): State<App>) -> Response {
    let Ok(permit) = app.inner.sse_slots.clone().try_acquire_owned() else {
        return sse_limit();
    };
    let stream = v2::stream(app.inner.exec.hub(), &app.inner.feed, permit);
    v2::events_response(futures_util::StreamExt::take_until(stream, app.stopped()))
}

/// `/v2/state`（V2-6）：调试用，内容同 snapshot。
async fn v2_state(State(app): State<App>) -> Response {
    v2::state_response(app.inner.exec.hub(), &app.inner.feed)
}

/// `/v2/screen`：触屏首页信号卡和状态栏的结论（screen.rs），从当前这份 /state 算，
/// 另加采样循环记的 30 秒收发包数（cell_window.rs，判 stall 用；不进 /state）。
/// 不冻结（不是旧接口），靠 `v` 区分版本；`ts` 就是算它用的那份快照的 `ts`。
async fn v2_screen(State(app): State<App>) -> Json<Value> {
    let snap = app.snapshot().await;
    let ts = snap.ts;
    let state = serde_json::to_value(&snap).unwrap_or(Value::Null);
    // E4 T13（V2-34、V2-38）：叠在首页的和 op 在同一把锁里算。
    let (screen_op, op) = app.inner.ops.screen();
    let net = crate::project::screen::net_view_op(
        &state,
        crate::cell_window::current(),
        screen_op.as_ref(),
    );
    Json(screen_json(ts, net, op, app.inner.exec.exec_age_ms()))
}

/// `/v2/screen` 的回复。`exec_age_ms`（V2-40）：触屏不订阅 `/v2/events`、看不到心跳，
/// 从这里判断 datad 卡没卡（这个请求读的是现成的快照，不经执行者，卡住时照样回）。
fn screen_json(
    ts: i64,
    net: crate::project::screen::NetView,
    op: Value,
    exec_age_ms: u64,
) -> Value {
    serde_json::json!({
        "v": crate::project::screen::SCREEN_VERSION,
        "ts": ts,
        "net": net,
        "op": op,
        "exec_age_ms": exec_age_ms,
    })
}

async fn control(
    State(app): State<App>,
    peer: Option<axum::Extension<LegacyPeer>>,
    method: Method,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let _ = method;
    let Json(body) = match payload {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":{"code":"invalid_request","message":"request body must be valid JSON"}})),
            )
                .into_response();
        }
    };
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":{"code":"invalid_request","message":"missing action"}})),
        )
            .into_response();
    }
    if body.get("params").is_some_and(|params| !params.is_object()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"action":action,"error":{"code":"invalid_request","message":"params must be a JSON object"}})),
        )
            .into_response();
    }
    let action = action.to_owned();
    crate::legacy_hits::hit(
        &crate::legacy_hits::control_key(&action, body.get("source").is_some()),
        peer.and_then(|p| p.0.0),
    )
    .await;
    write::handle(app, action, body).await
}

async fn shutdown(app: App) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    // 收到 SIGTERM/SIGINT：先让 SSE 流结束（P2-4），再收掉 ubus listen 的子进程（最多 2 秒），
    // 然后 axum 等连接收尾（`serve` 里另有总上限）。
    app.inner.stop.send_replace(true);
    let _ = tokio::task::spawn_blocking(|| crate::legacy_hits::flush(true)).await;
    crate::ubus::listen::shutdown(std::time::Duration::from_secs(2)).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn screen_reply_carries_executor_age() {
        let v = super::screen_json(
            7,
            crate::project::screen::NetView::default(),
            serde_json::json!({"active": null}),
            21_000,
        );
        assert_eq!(v["exec_age_ms"], 21_000);
        assert_eq!(v["ts"], 7);
        assert!(v["op"].is_object() && v["net"].is_object());
    }

    use super::*;
    use std::collections::HashSet;
    #[test]
    fn version_shape() {
        let v = serde_json::to_value(DatadVersion::default()).unwrap();
        assert_eq!(v["name"], "zwrt-datad");
    }

    #[test]
    fn capability_controls_are_the_kept_actions() {
        let controls = capability_controls();
        assert_eq!(controls.len(), 25);
        assert_eq!(controls.iter().copied().collect::<HashSet<_>>().len(), 25);
    }

    #[test]
    fn missing_static_token_never_disables_lan_authentication() {
        assert!(!static_token_valid(None, None));
        assert!(!static_token_valid(None, Some("anything")));
        assert!(static_token_valid(Some("secret"), Some("secret")));
        assert!(!static_token_valid(Some("secret"), Some("wrong")));
    }
}
