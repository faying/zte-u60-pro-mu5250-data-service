//! 事务的界面数据（E4 T13，docs/STATE_V2.md 第 12 节；manager `docs/designs/write-op-layer.md`
//! 设计评审的「状态文案表」，DD6、DD7、DD10、DD12、DD14、DD16）。
//!
//! 纯函数：给事务和上下文，出文字和标志。文字中英各一份，触屏和网页照着显示。
//! 中文排版：中文和数字、英文之间空一格，中文和中文之间不空（[`cat`]）。

use super::spec::NETWORK_MODE;
use super::txn::{Phase, Reason, Source, Txn};
use crate::screen::{ScreenOp, ScreenOpKind};
use serde_json::{Value, json};

type Words = (String, String);

/// DD14：来源显示名。
pub fn source_words(s: Source) -> (&'static str, &'static str) {
    match s {
        Source::Screen | Source::Legacy => ("触屏", "Screen"),
        Source::Web => ("网页", "Web"),
        Source::Scenario => ("情景", "Scene"),
        Source::Scheduler => ("定时任务", "Schedule"),
        Source::Auto => ("自动", "Auto"),
        Source::Guard => ("自动恢复", "Auto-recovery"),
    }
}

/// 一项的叫法：(进行中的大字, 项名, 英文项名)。描述表里现在只有网络模式，其余先用通用的。
fn item_words(item: &str) -> (&'static str, &'static str, &'static str) {
    match item {
        NETWORK_MODE => ("正在换制式", "制式", "Network mode"),
        "netselect.session" => ("正在搜网", "搜网", "Network search"),
        _ => ("正在改设置", "设置", "Setting"),
    }
}

/// 值的显示名：网络模式用首页同一张表（自动 / 只用 4G …）。
pub fn value_words(item: &str, v: &str) -> Words {
    match item {
        NETWORK_MODE => crate::screen::net_select_word(v),
        _ if v.is_empty() => ("-".into(), "-".into()),
        _ => (v.to_owned(), v.to_owned()),
    }
}

/// 中文拼接：两边挨着的是中文就不空格，否则空一格（「退回到自动」「退回到 4G + 5G」）。
fn cat(a: &str, b: &str) -> String {
    let wide = |c: Option<char>| c.is_some_and(|c| !c.is_ascii());
    if a.is_empty() || b.is_empty() || (wide(a.chars().last()) && wide(b.chars().next())) {
        format!("{a}{b}")
    } else {
        format!("{a} {b}")
    }
}

/// 倒计时 `m:ss`。
pub fn clock(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    format!("{}:{:02}", s / 60, s % 60)
}

/// 结果停留多久（DD16）。
fn stay(t: &Txn) -> &'static str {
    if !t.phase.is_final() {
        return "live";
    }
    match (t.phase, t.reason) {
        (Phase::RollbackFailed, _) => "alert",
        (Phase::Cancelled, Some(Reason::Superseded)) => "none",
        (Phase::Confirmed, Some(Reason::Verified | Reason::NoVerify))
        | (Phase::RolledBack, Some(Reason::UserRevert)) => "brief",
        _ => "sticky",
    }
}

/// 终态的一句话（状态文案表，不带符号）：(mark, 中, 英)。`x` 退回目标，`y` 目标，`old` 写之前的值。
fn final_words(t: &Txn, x: &Words, y: &Words, old: &Words) -> (&'static str, String, String) {
    use Phase::*;
    use Reason::*;
    let (mark, zh, en) = match (t.phase, t.reason) {
        (Confirmed, Some(UserKeep)) => (
            "warn",
            format!("{} · 没确认通", cat("保留", &y.0)),
            format!("Kept {} · unconfirmed", y.1),
        ),
        (Confirmed, Some(NoVerify)) => (
            "ok",
            "已改 · 没有可确认的读数".into(),
            "Saved · not checked".into(),
        ),
        (Confirmed, _) => ("ok", cat("已切到", &y.0), format!("Now {}", y.1)),
        (Unverified, Some(ApnNoData)) => (
            "warn",
            "APN 已存 · 数据关着没法试".into(),
            "APN saved · untested".into(),
        ),
        (Unverified, _) => (
            "bad",
            format!("没通 · {}", cat("还是", &y.0)),
            format!("No data · still {}", y.1),
        ),
        (RolledBack, Some(UserRevert)) => ("ok", cat("已退回", &x.0), format!("Back to {}", x.1)),
        (RolledBack, Some(RebootLoop)) => (
            "warn",
            format!("重启了两次 · {}", cat("已退回", &x.0)),
            format!("2 reboots · back to {}", x.1),
        ),
        (RolledBack, _) => (
            "warn",
            format!("没通 · {}", cat("已退回", &x.0)),
            format!("No data · back to {}", x.1),
        ),
        (NotApplied, _) => (
            "warn",
            format!("没切成 · {}", cat("还是", &old.0)),
            format!("Didn't apply · still {}", old.1),
        ),
        (RollbackFailed, _) => ("bad", "退回也没通".into(), "Revert failed".into()),
        (Cancelled, Some(Superseded)) => (
            "ok",
            "被新的改动接替".into(),
            "Replaced by a newer change".into(),
        ),
        (Cancelled, Some(Preempted)) => (
            "warn",
            "已关数据 · 不再自动退回".into(),
            "Data off · no auto revert".into(),
        ),
        (Cancelled, Some(Takeover)) => (
            "warn",
            "手动接管 · 不再自动退回".into(),
            "Manual override".into(),
        ),
        (Cancelled, Some(SimChanged)) => {
            ("warn", "换过卡 · 不再自动退回".into(), "SIM changed".into())
        }
        (Cancelled, _) => (
            "warn",
            "别处改过 · 不再自动退回".into(),
            "Changed elsewhere".into(),
        ),
        // 进行中的阶段不会走到这里。
        _ => ("warn", "已结束".into(), "Ended".into()),
    };
    (mark, zh, en)
}

/// 视图里要用、但事务自己不知道的：同一项后来又有别的事务（结束的或进行中的）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    pub later_change: bool,
}

/// DD10：能不能撤销，不能时的原因。
fn undo_denied(t: &Txn, ctx: Ctx) -> Option<(&'static str, &'static str)> {
    const SINCE: (&str, &str) = ("之后又改过", "Changed since");
    if ctx.later_change {
        return Some(SINCE);
    }
    match t.phase {
        Phase::Confirmed | Phase::Unverified => None,
        Phase::RolledBack | Phase::NotApplied => Some(("设置没变 · 不用撤销", "Nothing to undo")),
        Phase::RollbackFailed => Some(("先再试一次退回", "Retry the revert instead")),
        Phase::Cancelled => match t.reason {
            Some(Reason::SimChanged) => Some(("换过卡", "SIM changed")),
            Some(Reason::Superseded) => Some(SINCE),
            _ if t.readback.as_deref() == Some(t.target.as_str()) => None,
            _ => Some(SINCE),
        },
        _ => None,
    }
}

/// 进度三行（DD12）看的值：退回中、退回后看退回目标。
fn goal(t: &Txn) -> &str {
    match t.phase {
        Phase::RollingBack | Phase::RolledBack | Phase::RollbackFailed => &t.rollback_to,
        _ => &t.target,
    }
}

/// 事务视图：`Txn::status` 加上界面字段（V2-35、V2-36）。
pub fn view(t: &Txn, now: u64, ctx: Ctx) -> Value {
    let mut v = t.status(now);
    let item = t.item.as_str();
    let (doing, what_zh, what_en) = item_words(item);
    let (src_zh, src_en) = source_words(t.source);
    let x = value_words(item, &t.rollback_to);
    let y = value_words(item, &t.target);
    let old = value_words(item, &t.old);
    let readback = t.readback.as_deref().map(|r| value_words(item, r));
    let live = !t.phase.is_final();

    let (mark, say_zh, say_en) = match t.phase {
        Phase::Accepted | Phase::Applying => (None, doing.to_string(), "Switching".to_string()),
        Phase::Verifying => (None, "正在确认".into(), "Checking".into()),
        Phase::RollingBack => (None, "正在退回".into(), "Reverting".into()),
        _ => {
            let (m, zh, en) = final_words(t, &x, &y, &old);
            (Some(m), zh, en)
        }
    };
    let next = match t.phase {
        _ if !t.counting() => None,
        Phase::Verifying if t.rollback_enabled => Some((
            cat("{t} 后没通就退回到", &x.0),
            format!("Back to {} in {{t}} if no data", x.1),
        )),
        Phase::Verifying => Some((
            "还剩 {t} · 自动退回没开".to_string(),
            "{t} left · auto revert off".to_string(),
        )),
        _ => Some((
            format!("{} · 还剩 {{t}}", cat("退回到", &x.0)),
            format!("To {} · {{t}} left", x.1),
        )),
    };
    let note = (live && t.boots > 0).then_some(("重启过 · 重新确认", "Rebooted · checking again"));
    let applied = t.readback.as_deref() == Some(goal(t));
    let steps = json!([
        {"key": "applied", "zh": "设置已生效", "en": "Setting applied", "done": applied},
        {"key": "registered", "zh": "已注册", "en": "Registered", "done": t.registered},
        {"key": "data", "zh": "数据", "en": "Data", "done": t.data_ok},
    ]);
    let can = t.phase == Phase::Verifying && t.counting();
    let undo = if live {
        Value::Null
    } else {
        let (label_zh, label_en) = if t.undo {
            ("重做", "Redo")
        } else {
            ("撤销", "Undo")
        };
        let why = undo_denied(t, ctx);
        json!({
            "ok": why.is_none(),
            "label_zh": label_zh,
            "label_en": label_en,
            "why_zh": why.map(|w| w.0),
            "why_en": why.map(|w| w.1),
            "value": t.old,
        })
    };

    let m = v.as_object_mut().expect("status is an object");
    let mut put = |k: &str, val: Value| {
        m.insert(k.into(), val);
    };
    put("source_zh", json!(src_zh));
    put("source_en", json!(src_en));
    put("what_zh", json!(what_zh));
    put("what_en", json!(what_en));
    put("old_zh", json!(old.0));
    put("old_en", json!(old.1));
    put("target_zh", json!(y.0));
    put("target_en", json!(y.1));
    put("rollback_to_zh", json!(x.0));
    put("rollback_to_en", json!(x.1));
    put("readback_zh", json!(readback.as_ref().map(|r| &r.0)));
    put("readback_en", json!(readback.as_ref().map(|r| &r.1)));
    put("say_zh", json!(say_zh));
    put("say_en", json!(say_en));
    put("mark", json!(mark));
    put("stay", json!(stay(t)));
    put("next_zh", json!(next.as_ref().map(|n| &n.0)));
    put("next_en", json!(next.as_ref().map(|n| &n.1)));
    put("note_zh", json!(note.map(|n| n.0)));
    put("note_en", json!(note.map(|n| n.1)));
    put("steps", steps);
    put("can_revert", json!(can));
    put("can_keep", json!(can));
    put("undo", undo);
    v
}

/// 结束的事务要不要等「知道了」（常驻类）。
pub fn sticky(t: &Txn) -> bool {
    matches!(stay(t), "sticky" | "alert")
}

/// V2-39：busy 回复里的一句。`item` 为 None 时按动作名猜不出项，用通用的。
pub fn busy_words(item: &str, source: Source, age_ms: u64) -> Words {
    let (doing, _, what_en) = item_words(item);
    let (src_zh, src_en) = source_words(source);
    (
        format!("{doing}（{src_zh}发起，{} 秒），稍等", age_ms / 1000),
        format!("Busy: {} ({src_en})", what_en.to_ascii_lowercase()),
    )
}

/// 首页结论用的一行（V2-38）：进行中的事务，或者还没点「知道了」的常驻结果。
pub fn screen_op(active: Option<&Txn>, last: Option<(&Txn, bool)>, now: u64) -> Option<ScreenOp> {
    if let Some(t) = active {
        let v = view(t, now, Ctx::default());
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let t_left = clock(t.remaining_ms(now).unwrap_or(0));
        let (mut hint_zh, mut hint_en) = (
            s("next_zh").replace("{t}", &t_left),
            s("next_en").replace("{t}", &t_left),
        );
        if let (Some(nz), Some(ne)) = (v["note_zh"].as_str(), v["note_en"].as_str()) {
            hint_zh = if hint_zh.is_empty() {
                nz.into()
            } else {
                format!("{nz} · {hint_zh}")
            };
            hint_en = if hint_en.is_empty() {
                ne.into()
            } else {
                format!("{ne} · {hint_en}")
            };
        }
        return Some(ScreenOp {
            kind: ScreenOpKind::Live,
            head: (s("say_zh"), s("say_en")),
            hint: (hint_zh, hint_en),
        });
    }
    let (t, acked) = last?;
    if acked || !sticky(t) {
        return None;
    }
    let item = t.item.as_str();
    let (say_zh, say_en) = {
        let x = value_words(item, &t.rollback_to);
        let y = value_words(item, &t.target);
        let old = value_words(item, &t.old);
        let (_, zh, en) = final_words(t, &x, &y, &old);
        (zh, en)
    };
    if t.phase == Phase::RollbackFailed {
        let x = value_words(item, &t.rollback_to);
        let (now_zh, now_en) = match t.readback.as_deref() {
            Some(r) => {
                let w = value_words(item, r);
                (cat("现在是", &w.0), format!("Now {}", w.1))
            }
            None => ("当前设置未知".into(), "Current setting unknown".into()),
        };
        return Some(ScreenOp {
            kind: ScreenOpKind::Alert,
            head: ("退回也没通".into(), "Failed".into()),
            hint: (
                format!(
                    "{now_zh} · {} · 再试一次退回或重启设备",
                    cat("上次确认是", &x.0)
                ),
                format!("{now_en} · last good {} · retry revert or restart", x.1),
            ),
        });
    }
    Some(ScreenOp {
        kind: ScreenOpKind::Sticky,
        head: (String::new(), String::new()),
        hint: (say_zh, say_en),
    })
}

#[cfg(test)]
mod tests;
