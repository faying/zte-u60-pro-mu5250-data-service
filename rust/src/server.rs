use crate::{
    auth::{self, Sessions},
    block::{self, Hub},
    executor::{self, Executor, RoundDriver},
    model::{DatadVersion, Snapshot},
    neighbor_manager::Manager as NeighborManager,
    state, v2,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::rejection::JsonRejection,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use serde_json::{Map, Value, json};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore, watch},
};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tower_http::limit::RequestBodyLimitLayer;

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
    device_session: Mutex<Option<DeviceSession>>,
    /// `/events` 和 `/v2/events` 共用的 SSE 连接名额。
    sse_slots: Arc<Semaphore>,
    /// `/v2` 的流（epoch + broadcast），事件由 `Hub` 在锁里发进来。
    feed: Arc<v2::Feed>,
    neighbor: Mutex<NeighborManager>,
}

struct DeviceSession {
    _token: String,
    _password_hash: String,
}

impl App {
    pub async fn new(
        data_dir: PathBuf,
        interval: Duration,
        token: Option<String>,
        neighbor_enabled: bool,
    ) -> Result<Self> {
        let feed = v2::Feed::new(v2::CAPACITY);
        let hub = Arc::new(Hub::new(
            block::phase1_blocks(),
            Box::new(v2::FeedSink(feed.clone())),
        ));
        let cfg = executor::Config {
            cache: state::cache_enabled(),
            ..executor::Config::default()
        };
        let exec = Executor::spawn(
            crate::ubus::backend::Backend::from_env(),
            hub.clone(),
            cfg,
            interval,
        );
        executor::install(&exec);
        crate::cooling::tick().await;
        crate::extra_wifi::tick().await;
        // 第一轮在执行者里立即采（块 + 旧采集），之后执行者自己「睡一个采样间隔 → 一轮」。
        let interval_ms = interval.as_millis() as u64;
        let mut initial = exec
            .round_now(async move { state::collect(interval_ms, &hub).await })
            .await;
        let mut neighbor = NeighborManager::new(neighbor_enabled);
        neighbor
            .tick(initial.fields.get("net").unwrap_or(&Value::Null))
            .await;
        initial.fields.insert("neighbor".into(), neighbor.status());
        let (tx, _) = watch::channel(initial.clone());
        let app = Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(initial),
                tx,
                exec,
                neighbor: Mutex::new(neighbor),
                _data_dir: data_dir,
                token,
                sessions: Mutex::new(Sessions::default()),
                device_session: Mutex::new(None),
                sse_slots: Arc::new(Semaphore::new(16)),
                feed,
            }),
        };
        app.inner.exec.start_rounds(Arc::new(app.clone()));
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
        Ok(app)
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.read().await.clone()
    }
    /// 一轮里的旧采集（在执行者里跑，块已经读完）。别在持有 neighbor/snapshot 锁时调 ubus：
    /// 控制任务只在 `ubus_ttl` 这类安全点插进来，这里的锁段里没有安全点。
    async fn refresh_snapshot(&self) {
        crate::cooling::tick().await;
        crate::extra_wifi::tick().await;
        let mut next = state::collect(self.inner.exec.interval_ms(), self.inner.exec.hub()).await;
        let mut neighbor = self.inner.neighbor.lock().await;
        neighbor
            .tick(next.fields.get("net").unwrap_or(&Value::Null))
            .await;
        next.fields.insert("neighbor".into(), neighbor.status());
        drop(neighbor);
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
    pub async fn serve(
        self,
        addr: SocketAddr,
        require_auth: bool,
        open_auth_routes: bool,
    ) -> Result<()> {
        let mut router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(snapshot))
            .route("/events", get(events))
            .route("/v2/events", get(v2_events))
            .route("/v2/state", get(v2_state))
            .route("/capabilities", get(capabilities))
            .route("/control", post(control));
        if open_auth_routes {
            router = router
                .route("/auth/login", post(auth_login))
                .route("/auth/exchange", post(auth_exchange));
        }
        if require_auth {
            router = router.layer(middleware::from_fn_with_state(self.clone(), authenticate));
        }
        let router = router
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let listener = TcpListener::bind(addr).await?;
        let result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await;
        self.inner.neighbor.lock().await.shutdown().await;
        result?;
        Ok(())
    }
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
async fn health() -> &'static str {
    "ok\n"
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
async fn snapshot(State(app): State<App>) -> Json<Snapshot> {
    Json(app.snapshot().await)
}
fn capability_controls() -> Vec<&'static str> {
    let mut controls = vec![
        "device.login_info",
        "device.login",
        "device.logout",
        "device.session_status",
        "device.change_password",
        "wifi.status",
        "wifi.dual_band_status",
        "wifi.txpower.status",
        "wifi.advanced.status",
        "wireless.config",
        "sleep.status",
        "usb.status",
        "power.direct_supply.status",
        "apn.list",
        "client.access",
        "neighbor.status",
        "neighbor.set",
        "state.refresh",
        "state.set_interval",
        "qos.reload",
    ];
    controls.extend_from_slice(crate::control::ACTIONS);
    controls
}

async fn capabilities() -> Json<Value> {
    let controls = capability_controls();
    Json(json!({
        "schema_version":1,
        "protocol":1,
        "events":["state"],
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

async fn events(State(app): State<App>) -> Response {
    let Ok(permit) = app.inner.sse_slots.clone().try_acquire_owned() else {
        return sse_limit();
    };
    let stream = WatchStream::new(app.inner.tx.subscribe()).map(move |v| {
        let _keep_permit_alive = &permit;
        Ok::<_, Infallible>(Event::default().event("state").json_data(v).unwrap())
    });
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response()
}
/// `/v2/events`（STATE_V2.md 第 2–4 节）：和 `/events` 共用连接名额，满了同样 503。
async fn v2_events(State(app): State<App>) -> Response {
    let Ok(permit) = app.inner.sse_slots.clone().try_acquire_owned() else {
        return sse_limit();
    };
    v2::events_response(app.inner.exec.hub(), &app.inner.feed, permit)
}

/// `/v2/state`（V2-6）：调试用，内容同 snapshot。
async fn v2_state(State(app): State<App>) -> Response {
    v2::state_response(app.inner.exec.hub(), &app.inner.feed)
}

async fn control(
    State(app): State<App>,
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
    // 整个处理过程作为一个控制任务交给执行者（V2-24）：排队中的满 8 个立即 503（V2-25），
    // 否则挂到做完才回复（和原来一样；请求方断开了任务也做完）。
    let action = action.to_owned();
    let exec = app.inner.exec.clone();
    let marker = exec.clone();
    let task_action = action.clone();
    let task = async move {
        let response = control_task(app, &task_action, body).await;
        // V2-27：成功后相关块下一轮立即读（没有映射就全部块），不另起一轮采集。
        if response.status().is_success() && !read_only(&task_action) {
            crate::state::invalidate_cache();
            marker.mark_immediate(blocks_for_action(&task_action));
        }
        response
    };
    match exec.control(task).await {
        Ok(response) => response,
        Err(executor::Busy) => control_busy(&action),
    }
}

/// 只读、不改设备的动作：不清慢数据缓存、不标块（`sms.list_after` 翻页时每页一次，清缓存会让下一轮全部重读）。
fn read_only(action: &str) -> bool {
    action == "sms.list_after"
}

/// `/control` 动作 → 它会改变的块。没列出的动作算「没有映射」，成功后全部块立即读（R12）。
fn blocks_for_action(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "power.direct_supply.set" | "power.direct_supply.status" => Some(&["charger"]),
        "sms.list_after" => Some(&[]),
        _ => None,
    }
}

/// V2-25：控制队列满。
pub(crate) fn control_busy(action: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"ok":false,"action":action,"error":{"code":"busy","message":"control queue full"}})),
    )
        .into_response()
}

async fn control_task(app: App, action: &str, body: Value) -> Response {
    // Whatever this action changes, the cached slow-moving state may now be
    // stale (state.rs `invalidate_cache`); it is cleared again after a
    // successful action (the task runs on the executor, so no round can refill
    // the cache with pre-action values in between).
    if !read_only(action) {
        crate::state::invalidate_cache();
    }
    if action == "neighbor.status" {
        return (StatusCode::OK,Json(json!({"ok":true,"action":action,"result":app.inner.neighbor.lock().await.status()}))).into_response();
    }
    if action == "neighbor.set" {
        let Some(enabled) = neighbor_enabled(&body) else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"enabled must be boolean"}}))).into_response();
        };
        return match app.inner.neighbor.lock().await.set_enabled(enabled).await {Ok(value)=>(StatusCode::OK,Json(json!({"ok":true,"action":action,"result":value}))).into_response(),Err(error)=>(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}}))).into_response()};
    }
    if action == "device.login_info" {
        return readonly_ubus(action, "zwrt_web", "web_login_info", json!({})).await;
    }
    if action == "device.login" {
        let password_hash = body
            .get("params")
            .and_then(|value| value.get("password_hash"))
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        let Some(password_hash) = password_hash else {
            return invalid_parameter(
                action,
                "password_hash must be a 64 character SHA-256 hex value",
            );
        };
        let password_hash = password_hash.to_ascii_uppercase();
        return match state::ubus("zwrt_web", "web_login", json!({"password":password_hash})).await {
            Ok(value)
                if value.get("result").and_then(Value::as_i64) == Some(0)
                    && value
                        .get("ubus_rpc_session")
                        .and_then(Value::as_str)
                        .is_some_and(|token| !token.is_empty()) =>
            {
                let token = value["ubus_rpc_session"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                *app.inner.device_session.lock().await = Some(DeviceSession {
                    _token: token,
                    _password_hash: password_hash,
                });
                control_ok(action, value)
            }
            Ok(_) => control_failed(action, "device login rejected".into()),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "device.logout" {
        *app.inner.device_session.lock().await = None;
        return control_ok(action, json!({"logged_in":false}));
    }
    if action == "device.session_status" {
        return control_ok(
            action,
            json!({"logged_in":app.inner.device_session.lock().await.is_some()}),
        );
    }
    if action == "device.change_password" {
        let valid_hash = |name: &str| {
            body.get("params")
                .and_then(|v| v.get(name))
                .and_then(Value::as_str)
                .filter(|v| v.len() == 64 && v.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .map(|v| v.to_ascii_uppercase())
        };
        let (Some(old_hash), Some(new_hash)) = (valid_hash("old_hash"), valid_hash("new_hash"))
        else {
            return invalid_parameter(action, "old_hash and new_hash must be SHA-256 hex values");
        };
        return match state::ubus(
            "zwrt_web",
            "web_change_password",
            json!({"password_old":old_hash,"password_new":new_hash}),
        )
        .await
        {
            Ok(value) => {
                *app.inner.device_session.lock().await = None;
                control_ok(action, value)
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wifi.dual_band_status" {
        return match state::ubus("zwrt_router.api", "router_get_wifi_isolate", json!({})).await {
            Ok(value) => {
                let enabled = value
                    .get("wifimain24_wifimain5_enable")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    != 0;
                control_ok(
                    action,
                    json!({
                        "WiFiDualBandSupported":"1",
                        "WiFiDualBandEnabled":if enabled { "1" } else { "0" },
                        "BandSteeringSwitch":if enabled { "1" } else { "0" }
                    }),
                )
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wifi.status" {
        let mut result = serde_json::Map::new();
        for section in ["main_2g", "main_5g"] {
            let mut item = serde_json::Map::new();
            for field in ["ssid", "key", "encryption", "disabled"] {
                item.insert(
                    field.into(),
                    json!(state::uci_read(&format!("wireless.{section}.{field}")).await),
                );
            }
            result.insert(section.into(), Value::Object(item));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "wifi.txpower.status" {
        let model = state::uci_read("zwrt_common_info.common_config.model_name").await;
        let hardware = state::uci_read("zwrt_common_info.common_config.hardware_version").await;
        if model != "MU5252" && !hardware.starts_with("MU5252_") {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"wifi power control is only supported on MU5252"}}))).into_response();
        }
        let mut result = Map::new();
        for (band, section, factory_limit) in [("2g", "wifi0", 19), ("5g", "wifi1", 18)] {
            let mut values = Vec::new();
            for option in ["disabled", "txpowerpercent", "txpower", "max_power"] {
                let raw = state::uci_read(&format!("wireless.{section}.{option}")).await;
                let Ok(value) = raw.parse::<i64>() else {
                    return control_failed(
                        action,
                        format!("failed to read {band} wifi power configuration"),
                    );
                };
                values.push(value);
            }
            result.insert(band.into(), json!({"enabled":values[0]==0,"percent":values[1],"txpower_dbm":values[2],"limit_dbm":values[3],"factory_limit_dbm":factory_limit}));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "wifi.advanced.status" {
        return match crate::wifi::advanced_status().await {
            Ok(value) => control_ok(action, value),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wireless.config" {
        let mutating = body.get("params").is_some_and(|params| {
            params.get("country").is_some() || params.get("channel").is_some()
        });
        if !mutating {
            return match crate::wifi::wireless_config_status().await {
                Ok(value) => control_ok(action, value),
                Err(error) => control_failed(action, error),
            };
        }
    }
    if action == "sleep.status" {
        return control_ok(
            action,
            json!({
                "idle_seconds":state::uci_read("zwrt_sleep.ztmp_time.SysIdTime").await,
                "enabled":state::uci_read("zwrt_sleep.ztmp_switch.sleepSwitch").await,
                "wakeup":state::uci_read("zwrt_sleep.ztmp_switch.wakeupSwitch").await,
                "status":state::uci_read("zwrt_sleep.ztmp_status.sleepStatus").await,
            }),
        );
    }
    if action == "usb.status" {
        let typec = state::ubus("zwrt_bsp.typec", "list", json!({})).await;
        let usb = state::ubus("zwrt_bsp.usb", "list", json!({})).await;
        return match (typec, usb) {
            (Ok(typec), Ok(usb)) => control_ok(action, json!({"typec":typec,"usb":usb})),
            (Err(error), _) | (_, Err(error)) => control_failed(action, error),
        };
    }
    if action == "power.direct_supply.status" {
        return match state::ubus("zwrt_bsp.charger", "list", json!({})).await {
            Ok(value) => {
                let result = match value
                    .get("direct_power_supply_mode")
                    .and_then(Value::as_str)
                {
                    Some("enable") => json!({"supported":true,"enabled":true,"mode":"enable"}),
                    Some("disable") => json!({"supported":true,"enabled":false,"mode":"disable"}),
                    Some(_) => json!({"supported":true,"enabled":Value::Null,"mode":Value::Null}),
                    None => json!({"supported":false,"enabled":Value::Null,"mode":Value::Null}),
                };
                control_ok(action, result)
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "apn.list" {
        // 同一个控制任务里依次调完（V2-26）；四个都调，错误取第一个，和原来的 join 一样。
        let values = (
            state::ubus("zwrt_apn_object", "get_apn_mode", json!({})).await,
            state::ubus("zwrt_apn_object", "getAutoApnList", json!({})).await,
            state::ubus("zwrt_apn_object", "getManuApnList", json!({})).await,
            state::ubus("zwrt_apn_object", "get_enabled_manu_apn_id", json!({})).await,
        );
        return match values {
            (Ok(mode), Ok(automatic), Ok(manual), Ok(enabled)) => control_ok(
                action,
                json!({"mode":mode,"automatic":automatic,"manual":manual,"enabled":enabled}),
            ),
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => control_failed(action, error),
        };
    }
    if action == "client.access" {
        let values = (
            state::ubus(
                "uci",
                "get",
                json!({"config":"wireless","section":"main_2g"}),
            )
            .await,
            state::ubus(
                "zwrt_router.api",
                "router_lan_access_list",
                json!({"start_id":1,"end_id":64}),
            )
            .await,
            state::ubus(
                "zwrt_router.api",
                "router_wireless_access_list",
                json!({"start_id":1,"end_id":64}),
            )
            .await,
        );
        return match values {
            (Ok(policy), Ok(lan), Ok(wifi)) => {
                control_ok(action, json!({"policy":policy,"lan":lan,"wifi":wifi}))
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                control_failed(action, error)
            }
        };
    }
    match crate::control::execute(action, body.get("params").unwrap_or(&json!({}))).await {
        crate::control::Outcome::Ok(value) => {
            return control_ok(action, value);
        }
        crate::control::Outcome::Invalid(error) => return invalid_parameter(action, &error),
        crate::control::Outcome::Failed(error) => return control_failed(action, error),
        crate::control::Outcome::NotHandled => {}
    }
    if action == "state.refresh" {
        return control_ok(action, json!({"queued":true}));
    }
    if action == "state.set_interval" {
        let milliseconds = body
            .get("params")
            .and_then(|value| value.get("milliseconds"))
            .and_then(Value::as_u64);
        let Some(milliseconds) = milliseconds.filter(|value| (500..=5000).contains(value)) else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"milliseconds must be between 500 and 5000"}}))).into_response();
        };
        app.inner.exec.set_interval_ms(milliseconds);
        return control_ok(action, json!({"sample_interval_ms":milliseconds}));
    }
    if action == "qos.reload" {
        return control_ok(action, json!({"queued":true}));
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"ok":false,"action":action,"error":{"code":"unknown_action","message":"unsupported control action"}})),
    )
        .into_response()
}

/// `neighbor.set` 的 `enabled`：布尔或 0/1（CONTROL_API.md），和其他动作同一个 `boolean()`。
fn neighbor_enabled(body: &Value) -> Option<bool> {
    let empty = json!({});
    let params = body
        .get("params")
        .filter(|p| p.is_object())
        .unwrap_or(&empty);
    crate::control::boolean(params, "enabled").ok()
}

fn control_ok(action: &str, value: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"ok":true,"action":action,"result":value})),
    )
        .into_response()
}

fn control_failed(action: &str, error: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}})),
    )
        .into_response()
}

fn invalid_parameter(action: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":message}})),
    )
        .into_response()
}

async fn readonly_ubus(action: &str, service: &str, method: &str, args: Value) -> Response {
    match state::ubus(service, method, args).await {
        Ok(value) => control_ok(action, value),
        Err(error) => control_failed(action, error),
    }
}
async fn shutdown() {
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
    // 收到 SIGTERM/SIGINT：先收掉 ubus listen 子进程，再等连接收尾。
    crate::ubus::listen::shutdown(std::time::Duration::from_secs(2)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn version_shape() {
        let v = serde_json::to_value(DatadVersion::default()).unwrap();
        assert_eq!(v["name"], "zwrt-datad");
    }

    #[test]
    fn capability_controls_match_complete_legacy_count() {
        let controls = capability_controls();
        assert_eq!(controls.len(), 80);
        assert_eq!(controls.iter().copied().collect::<HashSet<_>>().len(), 80);
    }

    #[test]
    fn neighbor_set_enabled_accepts_bool_and_01() {
        for (v, want) in [
            (json!(true), Some(true)),
            (json!(false), Some(false)),
            (json!(1), Some(true)),
            (json!(0), Some(false)),
            (json!(2), None),
            (json!("1"), None),
            (json!(null), None),
        ] {
            assert_eq!(
                neighbor_enabled(&json!({"params":{"enabled":v}})),
                want,
                "{v}"
            );
        }
        assert_eq!(neighbor_enabled(&json!({})), None);
    }

    #[test]
    fn missing_static_token_never_disables_lan_authentication() {
        assert!(!static_token_valid(None, None));
        assert!(!static_token_valid(None, Some("anything")));
        assert!(static_token_valid(Some("secret"), Some("secret")));
        assert!(!static_token_valid(Some("secret"), Some("wrong")));
    }
}
