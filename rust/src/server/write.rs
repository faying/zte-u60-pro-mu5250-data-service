//! 写操作的处理（Phase 2a，D12）：`POST /control` 解析完请求体之后的全部——启动等待、E4 事务动作、
//! 旧请求排队、交给执行者、跨进程写锁、写操作日志、成功后让相关块立即重读。
//! HTTP 那层（解析、校验请求体、旧接口计数）在 `server.rs` 的 `control`；动作本身在 `crate::control`、`crate::ops`。
use super::{App, CONTROL_START_WAIT, Stage};
use crate::executor;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// `/control` 的一个请求（已确认是 JSON 对象、有 action、params 是对象）。
pub(super) async fn handle(app: App, action: String, body: Value) -> Response {
    // P1-3：启动中（第一轮、事务恢复还没做完）先等，最多 20 秒；还没好就照队列满回 503 `busy`。
    if !app.wait_stage(Stage::Ready, CONTROL_START_WAIT).await {
        return control_busy(&action);
    }
    if let Some(response) = ops_route(&app, &action, &body).await {
        return response;
    }
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let safety = crate::ops::spec::is_safety(&action, &params);
    let has_source = body.get("source").is_some();
    // 没有 source 的旧请求：锁被占时（描述表里的动作）回成功、排队；同一项当覆盖；关数据马上插队（D14）。
    let legacy_item = (!has_source)
        .then(|| crate::ops::spec::find(&action))
        .flatten()
        .filter(|s| s.target(&params).is_ok())
        .map(|s| s.item);
    if has_source {
        if safety {
            app.inner.ops.preempt();
        }
    } else if app.inner.ops.legacy_gate(legacy_item, safety) == crate::ops::LegacyGate::Queue {
        let item = legacy_item.expect("only described actions queue");
        app.inner.ops.enqueue_legacy(item, &action, params);
        return legacy_queued(&action);
    }
    if matches!(action.as_str(), "device.reboot" | "device.poweroff") {
        app.inner.ops.clear_legacy(&action);
    }
    match run_control(app.clone(), action.clone(), body.clone()).await {
        Ok(response) => response,
        // 执行者队列满（V2-25）。关数据（新旧客户端一样，D14）作为内部任务马上做；
        // 旧请求不回 503：描述表里的动作回成功、排队。
        Err(executor::Busy) if safety => {
            let exec = app.inner.exec.clone();
            exec.task(control_job(app, action, body)).await
        }
        Err(executor::Busy) => match legacy_item {
            Some(item) => {
                app.inner.ops.enqueue_legacy(item, &action, params);
                legacy_queued(&action)
            }
            None => control_busy(&action),
        },
    }
}

/// 排进旧请求队列的旧请求：回和今天一样形状的成功（原厂设置调用回 `{"result":"success"}`）。
fn legacy_queued(action: &str) -> Response {
    control_ok(action, json!({"result":"success"}))
}

/// 整个处理过程作为一个控制任务交给执行者（V2-24）：排队中的满 8 个立即 `Busy`（V2-25），
/// 否则挂到做完才回复（和原来一样；请求方断开了任务也做完）。
pub(super) async fn run_control(
    app: App,
    action: String,
    body: Value,
) -> Result<Response, executor::Busy> {
    let exec = app.inner.exec.clone();
    exec.control(control_job(app, action, body)).await
}

async fn control_job(app: App, action: String, body: Value) -> Response {
    let marker = app.inner.exec.clone();
    let writes = (crate::control::ACTIONS.contains(&action.as_str())
        || crate::control::E4_ACTIONS.contains(&action.as_str()))
        && !read_only(&action);
    if writes && let Some(response) = other_write_gate(&app, &action, &body) {
        return response;
    }
    // D29：会改设备的动作在执行者里拿着跨进程写锁做（和应急直写脚本互斥）。
    let _lock = if writes {
        Some(crate::ops::write_lock::acquire().await)
    } else {
        None
    };
    let journal = writes.then(|| JournalWrite::new(&action, &body)).flatten();
    let ends = ends_device(&action, &body);
    // 重启、关机、恢复出厂：先把这一行写进闪存再做（之后没机会了）。
    if let Some(j) = &journal
        && ends
    {
        j.line(&app, "requested", None);
        app.inner.ops.record().flush().await;
    }
    let response = control_task(app.clone(), &action, body).await;
    if let Some(j) = &journal
        && !ends
    {
        let ok = response.status().is_success();
        j.line(
            &app,
            if ok { "ok" } else { "failed" },
            (!ok).then_some(response.status().as_u16()),
        );
        if ok {
            j.owner(&app);
        }
    }
    // V2-27：成功后相关块下一轮立即读（没有映射就全部块），不另起一轮采集。
    if response.status().is_success() && !read_only(&action) {
        crate::state::invalidate_cache();
        marker.mark_immediate(blocks_for_action(&action));
    }
    response
}

/// D40：事务在确认或退回中时，影响上网的写（安全类写除外，照旧插队）按来源处理：用户的（screen、web、
/// 没有 source 的旧请求）照做并取消这次自动退回（other_change）；自动来源的回 409 `op_busy`，不做、不记账。
/// 在执行者里、真要做之前判断（执行者队列满回 503 的请求不会取消事务）。
fn other_write_gate(app: &App, action: &str, body: &Value) -> Option<Response> {
    let empty = json!({});
    let params = body.get("params").unwrap_or(&empty);
    if crate::ops::spec::is_safety(action, params)
        || !crate::ops::spec::affects_network(action, params)
    {
        return None;
    }
    let source = body
        .get("source")
        .and_then(Value::as_str)
        .and_then(crate::ops::Source::parse)
        .unwrap_or(crate::ops::Source::Legacy);
    let op = app.inner.ops.other_write(source).err()?;
    Some(op_busy(action, op))
}

/// D40 的 409：`{"ok":false,"action":…,"error":{"code":"op_busy","message":…,"op":{op_id,item,phase}}}`。
fn op_busy(action: &str, op: Value) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"ok":false,"action":action,"error":{
            "code":"op_busy",
            "message":"a change is being confirmed; try again after it ends",
            "op":op,
        }})),
    )
        .into_response()
}

/// 不走事务的写记一行流水账（T5）：旧请求直接执行的、带来源但不在描述表里的。
/// `sms.mark_read` 太频繁，不记；短信动作只记动作名（`record::redact`）。
struct JournalWrite {
    action: String,
    source: crate::ops::Source,
    params: Value,
    /// 描述表里的动作：改的项和目标值（成功后记 owners）。
    item: Option<(&'static str, String)>,
}

impl JournalWrite {
    fn new(action: &str, body: &Value) -> Option<Self> {
        if action == "sms.mark_read" {
            return None;
        }
        let empty = json!({});
        let params = body.get("params").unwrap_or(&empty);
        let source = body
            .get("source")
            .and_then(Value::as_str)
            .and_then(crate::ops::Source::parse)
            .unwrap_or(crate::ops::Source::Legacy);
        let item =
            crate::ops::spec::find(action).and_then(|s| s.target(params).ok().map(|v| (s.item, v)));
        Some(Self {
            action: action.to_owned(),
            source,
            params: crate::ops::record::redact(action, params),
            item,
        })
    }

    fn line(&self, app: &App, result: &str, status: Option<u16>) {
        app.inner.ops.record().append(json!({
            "action": self.action,
            "item": self.item.as_ref().map(|i| i.0),
            "source": self.source,
            "params": self.params,
            "result": result,
            "status": status,
        }));
    }

    fn owner(&self, app: &App) {
        if let Some((item, value)) = &self.item {
            app.inner
                .ops
                .record()
                .set_owner(item, self.source, false, value, None);
        }
    }
}

/// `journal.append`（只记账，T5）：eSIM、CHILL 这类不经 `/control` 的改动由 agent 补记。
/// 要顶层 `source`；`params.result` 必填，`skipped` 按 D27 合并。不受事务锁、不进执行者。
fn journal_append(app: &App, action: &str, body: &Value, params: &Value) -> Response {
    let Some(source) = body
        .get("source")
        .and_then(Value::as_str)
        .and_then(crate::ops::Source::parse)
    else {
        return invalid_parameter(action, "journal.append needs a known top-level source");
    };
    let Some(result) = params
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return invalid_parameter(action, "missing parameter: result");
    };
    let Some(what) = params
        .get("item")
        .or_else(|| params.get("action"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return invalid_parameter(action, "missing parameter: item or action");
    };
    let record = app.inner.ops.record();
    if result == "skipped" {
        let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
        record.skip(source.as_str(), what, reason);
    } else {
        let mut line = crate::ops::record::redact(what, params);
        line["source"] = json!(source);
        line["journal_append"] = json!(true);
        record.append(line);
    }
    control_ok(action, json!({"recorded":true}))
}

/// E4：`op.status` / `op.revert` / `op.keep`，以及带 source 的、描述表里的写（走事务）。
/// 其他请求返回 None，照原来的处理。
async fn ops_route(app: &App, action: &str, body: &Value) -> Option<Response> {
    let ops = &app.inner.ops;
    let empty = json!({});
    let params = body.get("params").unwrap_or(&empty);
    let op_id = params.get("op_id").and_then(Value::as_str);
    match action {
        "op.status" => return Some(control_ok(action, ops.status(op_id))),
        "journal.append" => return Some(journal_append(app, action, body, params)),
        "journal.list" => {
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 500) as usize;
            // 先等前面接受的行落盘，刚记的也读得到。
            ops.record().flush().await;
            // E4 T8c（V2-41）：每行加上界面直接显示的字段
            let mut entries = ops.record().list(limit);
            crate::ops::journal_view::decorate(&mut entries);
            return Some(control_ok(
                action,
                json!({"entries": entries, "owners": ops.record().owners()}),
            ));
        }
        "op.ack" => {
            let Some(op_id) = op_id else {
                return Some(invalid_parameter(action, "missing parameter: op_id"));
            };
            // 「知道了」是用户点的：只认触屏和网页（V2-37）。
            let source = body
                .get("source")
                .and_then(Value::as_str)
                .and_then(crate::ops::Source::parse)
                .filter(|s| matches!(s, crate::ops::Source::Screen | crate::ops::Source::Web));
            let Some(source) = source else {
                return Some(invalid_parameter(
                    action,
                    "op.ack needs source screen or web",
                ));
            };
            return Some(match ops.ack(op_id, source) {
                Ok(v) => control_ok(action, v),
                Err(e) => op_error(action, StatusCode::CONFLICT, "invalid_state", &e, None),
            });
        }
        // D40：agent 里不经 datad 的用户写（eSIM 切换、AT 终端）之前发，取消进行中的自动退回。
        // DD18：「自动退回已打开」的提示点「知道了」。都只认触屏和网页。
        "op.interrupt" | "op.notice_ack" => {
            let what = params
                .get("what")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            if action == "op.interrupt" && what.is_none() {
                return Some(invalid_parameter(action, "missing parameter: what"));
            }
            let source = body
                .get("source")
                .and_then(Value::as_str)
                .and_then(crate::ops::Source::parse)
                .filter(|s| matches!(s, crate::ops::Source::Screen | crate::ops::Source::Web));
            let Some(source) = source else {
                return Some(invalid_parameter(
                    action,
                    &format!("{action} needs source screen or web"),
                ));
            };
            return Some(control_ok(
                action,
                match what {
                    Some(what) if action == "op.interrupt" => ops.interrupt(source, what),
                    _ => ops.notice_ack(source),
                },
            ));
        }
        "op.revert" | "op.keep" => {
            let Some(op_id) = op_id else {
                return Some(invalid_parameter(action, "missing parameter: op_id"));
            };
            let r = if action == "op.revert" {
                ops.revert(op_id)
            } else {
                ops.keep(op_id)
            };
            return Some(match r {
                Ok(v) => control_ok(action, v),
                Err(e) => op_error(action, StatusCode::CONFLICT, "invalid_state", &e, None),
            });
        }
        _ => {}
    }
    // 搜网会话和它的步骤只给带来源的新客户端（D17）
    let session_action =
        action.starts_with("netselect.") || crate::ops::engine::SESSION_OPTIONAL.contains(&action);
    if session_action && body.get("source").is_none() {
        return Some(op_error(
            action,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "source required",
            None,
        ));
    }
    let source = body.get("source")?;
    let Some(source) = source.as_str().and_then(crate::ops::Source::parse) else {
        return Some(op_error(
            action,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "unknown source",
            None,
        ));
    };
    let session_err = |e: crate::ops::engine::SessionError| match e {
        crate::ops::engine::SessionError::Busy(doing) => op_error(
            action,
            StatusCode::CONFLICT,
            "busy",
            "another change is in progress",
            Some(("doing", doing)),
        ),
        crate::ops::engine::SessionError::Gone => op_error(
            action,
            StatusCode::CONFLICT,
            "invalid_state",
            "no such session (ended or expired)",
            None,
        ),
    };
    let session_id = |v: &Value| v.get("session").and_then(Value::as_str).map(str::to_owned);
    match action {
        "netselect.session.open" => {
            return Some(match ops.session_open(source) {
                Ok(v) => control_ok(action, v),
                Err(e) => session_err(e),
            });
        }
        "netselect.session.renew" | "netselect.session.close" => {
            let Some(id) = session_id(params) else {
                return Some(invalid_parameter(action, "missing parameter: session"));
            };
            let r = if action.ends_with("renew") {
                ops.session_renew(&id)
            } else {
                let result = params.get("result").and_then(Value::as_str);
                ops.session_close(&id, result)
                    .map(|()| json!({"session": id}))
            };
            return Some(match r {
                Ok(v) => control_ok(action, v),
                Err(e) => session_err(e),
            });
        }
        _ => {}
    }
    // 关数据 / 关漫游（D14）在会话期间也照做：在国外这是最要紧的写
    if !crate::ops::spec::is_safety(action, params)
        && let Err(e) = ops.session_gate(action, session_id(body).as_deref())
    {
        return Some(session_err(e));
    }
    let spec = crate::ops::spec::find(action)?;
    let target = match spec.target(params) {
        Ok(v) => v,
        Err(e) => return Some(invalid_parameter(action, &e)),
    };
    let op_id = match body.get("op_id") {
        None => None,
        Some(Value::String(s)) if crate::ops::engine::valid_op_id(s) => Some(s.clone()),
        Some(_) => {
            return Some(op_error(
                action,
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "op_id must be 1-64 letters, digits, '.', '_' or '-'",
                None,
            ));
        }
    };
    let undo = body.get("undo").and_then(Value::as_bool).unwrap_or(false);
    let submitted = ops
        .submit(crate::ops::Request {
            spec,
            target,
            source,
            op_id,
            undo,
        })
        .await;
    Some(match submitted {
        crate::ops::Submit::Existing(op) => (
            StatusCode::OK,
            Json(json!({"ok":true,"action":action,"op":op})),
        )
            .into_response(),
        crate::ops::Submit::Busy(doing) => op_error(
            action,
            StatusCode::CONFLICT,
            "busy",
            "another change is in progress",
            Some(("doing", doing)),
        ),
        crate::ops::Submit::NoCapture(e) => control_failed(action, e),
        crate::ops::Submit::Applied { result: Ok(v), op } => (
            StatusCode::OK,
            Json(json!({"ok":true,"action":action,"result":v,"op":op})),
        )
            .into_response(),
        crate::ops::Submit::Applied { result: Err(e), op } => op_error(
            action,
            StatusCode::BAD_GATEWAY,
            "device_call_failed",
            &e,
            Some(("op", op)),
        ),
    })
}

fn op_error(
    action: &str,
    status: StatusCode,
    code: &str,
    message: &str,
    extra: Option<(&str, Value)>,
) -> Response {
    let mut body = json!({"ok":false,"action":action,"error":{"code":code,"message":message}});
    if let Some((k, v)) = extra {
        body[k] = v;
    }
    (status, Json(body)).into_response()
}

/// 做完设备就没了（重启、关机、恢复出厂）：流水账先记 requested 并落盘。
fn ends_device(action: &str, body: &Value) -> bool {
    if matches!(action, "device.reboot" | "device.poweroff") {
        return true;
    }
    let p = &body["params"];
    action == "vendor.call"
        && matches!(
            (p["object"].as_str(), p["method"].as_str()),
            (Some("zwrt_mc.device.manager"), Some("device_reset"))
                | (Some("system"), Some("reboot"))
        )
}

/// 只读、不改设备的动作：不清慢数据缓存、不标块（`sms.list_after` 翻页时每页一次，清缓存会让下一轮全部重读）。
fn read_only(action: &str) -> bool {
    action == "sms.list_after"
}

/// `/control` 动作 → 它会改变的块。没列出的动作算「没有映射」，成功后全部块立即读（R12）。
fn blocks_for_action(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "power.direct_supply.set" => Some(&["charger"]),
        "usb.attach_mode" => Some(&["typec", "powerbank", "charger"]),
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
    match crate::control::execute(action, body.get("params").unwrap_or(&json!({}))).await {
        crate::control::Outcome::Ok(value) => {
            return control_ok(action, value);
        }
        crate::control::Outcome::Invalid(error) => return invalid_parameter(action, &error),
        crate::control::Outcome::Failed(error) => return control_failed(action, error),
        crate::control::Outcome::NotHandled => {}
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
    (
        StatusCode::NOT_FOUND,
        Json(json!({"ok":false,"action":action,"error":{"code":"unknown_action","message":"unsupported control action"}})),
    )
        .into_response()
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
