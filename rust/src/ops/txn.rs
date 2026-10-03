//! E4 写操作层的事务状态机（manager `docs/designs/write-op-layer.md`「状态表」，D13、D31、D32、D35）。
//!
//! 纯逻辑：不碰设备、不读时钟、不落盘。驱动（`engine.rs`）把写之后才采到的新鲜读数和时钟
//! （BOOTTIME 毫秒，跨 datad 重启连续）喂进来，按返回的 [`Next`] 去写设备、落盘。
//!
//! 进行中：accepted → applying → verifying →（没通）rolling_back → 终态。终态都放锁：
//!
//! | 终态 | 原因 |
//! |---|---|
//! | confirmed | verified / user_keep |
//! | unverified | no_rollback（自动退回关着，到点没通） |
//! | rolled_back | timeout / user_revert / reboot_loop |
//! | not_applied | ignored（读回从没变成过目标值） |
//! | rollback_failed | rollback_timeout |
//! | cancelled | superseded / preempted / manual_change / takeover / sim_changed |

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 谁发起的写。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Screen,
    Web,
    Legacy,
    Guard,
    Scenario,
    Scheduler,
    Auto,
}

impl Source {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "screen" => Self::Screen,
            "web" => Self::Web,
            "legacy" => Self::Legacy,
            "guard" => Self::Guard,
            "scenario" => Self::Scenario,
            "scheduler" => Self::Scheduler,
            "auto" => Self::Auto,
            _ => return None,
        })
    }

    /// D16：screen、web（含 undo）、legacy 算用户写；guard 和其他自动来源不算。
    pub fn is_user(self) -> bool {
        matches!(self, Self::Screen | Self::Web | Self::Legacy)
    }

    /// 能不能覆盖同一项进行中的事务：用户写可以；guard 只能覆盖同一项（D15，调用方已比过项）。
    pub fn may_override(self) -> bool {
        self.is_user() || self == Self::Guard
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Applying,
    Verifying,
    RollingBack,
    Confirmed,
    Unverified,
    RolledBack,
    NotApplied,
    RollbackFailed,
    Cancelled,
}

impl Phase {
    pub fn is_final(self) -> bool {
        !matches!(
            self,
            Self::Accepted | Self::Applying | Self::Verifying | Self::RollingBack
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Verified,
    UserKeep,
    NoRollback,
    Timeout,
    UserRevert,
    RebootLoop,
    Ignored,
    RollbackTimeout,
    Superseded,
    Preempted,
    ManualChange,
    Takeover,
    SimChanged,
}

/// 完整 SIM 身份（D32）：完整 ICCID + 卡槽。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimId {
    pub iccid: String,
    pub slot: i64,
}

impl SimId {
    /// ICCID 空或卡槽没报：没卡，或者基带重新注册时字段临时空着。
    fn blank(&self) -> bool {
        self.iccid.is_empty() || self.slot == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimCheck {
    Same,
    Changed,
    /// 这次读到的身份是空的（写之前有卡）：这个读数不拿来判断，等下一拍。
    Unknown,
}

/// D32：写之前的 SIM 身份和这次读到的比。只有「读到一个不同的非空身份」才算换卡
/// （写之前没卡、现在有卡也算）；读到空的不算换卡，只是这一拍不判断。
pub fn sim_check(captured: &SimId, now: &SimId) -> SimCheck {
    match (captured.blank(), now.blank()) {
        (true, true) => SimCheck::Same,
        (false, true) => SimCheck::Unknown,
        _ if captured == now => SimCheck::Same,
        _ => SimCheck::Changed,
    }
}

/// 一次新鲜读数：配置读回（读不到为 None）、是否已注册、当前 SIM。
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub value: Option<String>,
    pub registered: bool,
    pub sim: SimId,
}

/// 落盘的「意图」：先落意图、再动手、再落结果（D31）。重启时遇到意图没清，不重做，按读回判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Apply,
    Rollback,
}

/// 驱动下一步要做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Wait,
    /// 到了终态。
    Done,
    /// 把这个值写回去（退回）；意图已经记在事务里，写之前先落盘。
    Rollback(String),
}

/// 新事务的参数。
pub struct NewTxn {
    pub op_id: String,
    pub action: String,
    pub item: String,
    pub source: Source,
    pub undo: bool,
    pub target: String,
    /// 写之前的读回。
    pub old: String,
    /// 退回目标：没覆盖别的事务时等于 old；覆盖写继承被覆盖事务的退回目标（D35）。
    pub rollback_to: String,
    pub sim: SimId,
    pub rollback_enabled: bool,
    pub deadline_ms: u64,
    pub boot_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Txn {
    pub op_id: String,
    pub action: String,
    pub item: String,
    pub source: Source,
    pub undo: bool,
    pub target: String,
    pub old: String,
    pub rollback_to: String,
    pub phase: Phase,
    pub reason: Option<Reason>,
    /// 退回的起因（timeout / user_revert / reboot_loop），退回成功时就是终态原因。
    pub rollback_reason: Option<Reason>,
    /// 读回曾经对上过目标值（没对上过 = 写没生效；对上过又回旧值 = 带外改回）。
    pub ever_matched: bool,
    /// 写之后拿到过至少一次读数（一次都没有时到点不能判 not_applied）。
    pub read_ok: bool,
    /// 写调用本身报错了（读回仍是旧值就马上判 not_applied，不用等到点）。
    pub apply_failed: bool,
    pub intent: Option<Intent>,
    pub sim: SimId,
    pub rollback_enabled: bool,
    pub deadline_ms: u64,
    pub boot_id: String,
    /// 写之后经历的整机重启次数（D13）。
    pub boots: u32,
    /// 以前几次开机里已经等过的时长（D13 累计）。
    pub elapsed_prev_ms: u64,
    /// 当前这段等待（verifying 或 rolling_back）在这次开机里从哪一刻算起。
    pub wait_start_ms: u64,
    /// 最近一次喂读数或落盘时的时钟（整机重启后据此算上一次开机等了多久）。
    pub seen_ms: u64,
    pub created_ms: u64,
    /// 每次阶段变化 +1：驱动丢掉阶段变化之前发出去的读数。
    pub generation: u64,
}

impl Txn {
    pub fn new(n: NewTxn, now: u64) -> Self {
        Self {
            op_id: n.op_id,
            action: n.action,
            item: n.item,
            source: n.source,
            undo: n.undo,
            target: n.target,
            old: n.old,
            rollback_to: n.rollback_to,
            phase: Phase::Accepted,
            reason: None,
            rollback_reason: None,
            ever_matched: false,
            read_ok: false,
            apply_failed: false,
            intent: None,
            sim: n.sim,
            rollback_enabled: n.rollback_enabled,
            deadline_ms: n.deadline_ms,
            boot_id: n.boot_id,
            boots: 0,
            elapsed_prev_ms: 0,
            wait_start_ms: now,
            seen_ms: now,
            created_ms: now,
            generation: 0,
        }
    }

    fn step(&mut self, phase: Phase) {
        self.phase = phase;
        self.generation += 1;
    }

    fn finish(&mut self, phase: Phase, reason: Reason) -> Next {
        self.step(phase);
        self.reason = Some(reason);
        self.intent = None;
        Next::Done
    }

    fn waiting(&self) -> bool {
        matches!(self.phase, Phase::Verifying | Phase::RollingBack)
    }

    /// 动手写之前：落盘意图。
    pub fn begin_apply(&mut self) {
        self.step(Phase::Applying);
        self.intent = Some(Intent::Apply);
    }

    /// 写调用回来了（成功或报错），开始等确认。
    pub fn applied(&mut self, ok: bool, now: u64) {
        if self.phase != Phase::Applying {
            return;
        }
        self.intent = None;
        self.apply_failed = !ok;
        self.wait_start_ms = now;
        self.seen_ms = now;
        self.step(Phase::Verifying);
    }

    fn start_rollback(&mut self, why: Reason) -> Next {
        self.step(Phase::RollingBack);
        self.rollback_reason = Some(why);
        self.intent = Some(Intent::Rollback);
        Next::Rollback(self.rollback_to.clone())
    }

    /// 退回的写调用回来了：退回只发一次，之后按读回判断。
    pub fn rollback_sent(&mut self, now: u64) {
        if self.phase != Phase::RollingBack {
            return;
        }
        self.intent = None;
        self.wait_start_ms = now;
        self.seen_ms = now;
        self.generation += 1;
    }

    /// 到点没确认：自动退回开着就退，关着就以 unverified/no_rollback 结束（D30）。
    fn expire(&mut self, why: Reason) -> Next {
        if self.rollback_enabled {
            self.start_rollback(why)
        } else {
            self.finish(Phase::Unverified, Reason::NoRollback)
        }
    }

    /// 喂一次写之后才采到的读数。
    pub fn on_reading(&mut self, r: &Reading, now: u64) -> Next {
        if self.phase.is_final() {
            return Next::Done;
        }
        if !self.waiting() || self.intent.is_some() {
            return Next::Wait;
        }
        self.seen_ms = now;
        // D32：SIM 变了就放弃，不往别的卡上写旧卡的值；身份临时读空的这一拍只看时限。
        match sim_check(&self.sim, &r.sim) {
            SimCheck::Changed => return self.finish(Phase::Cancelled, Reason::SimChanged),
            SimCheck::Unknown => return self.on_tick(now),
            SimCheck::Same => {}
        }
        if let Some(v) = r.value.as_deref() {
            self.read_ok = true;
            if self.phase == Phase::Verifying {
                if v == self.target {
                    self.ever_matched = true;
                    if r.registered {
                        return self.finish(Phase::Confirmed, Reason::Verified);
                    }
                } else if v == self.old {
                    // R3：对上过又回到旧值是带外改回；从没对上过、写又报了错，就是没写进去。
                    if self.ever_matched {
                        return self.finish(Phase::Cancelled, Reason::ManualChange);
                    }
                    if self.apply_failed {
                        return self.finish(Phase::NotApplied, Reason::Ignored);
                    }
                } else if !v.is_empty() {
                    // R3：既不是旧值也不是目标值的第三个值。
                    return self.finish(Phase::Cancelled, Reason::ManualChange);
                }
            } else if v == self.rollback_to {
                if r.registered {
                    let why = self.rollback_reason.unwrap_or(Reason::Timeout);
                    return self.finish(Phase::RolledBack, why);
                }
            } else if v != self.target && !v.is_empty() {
                return self.finish(Phase::Cancelled, Reason::ManualChange);
            }
        }
        self.on_tick(now)
    }

    /// 这一拍没拿到读数（读失败）：只看时限。
    pub fn on_tick(&mut self, now: u64) -> Next {
        if self.phase.is_final() {
            return Next::Done;
        }
        if !self.waiting() || self.intent.is_some() {
            return Next::Wait;
        }
        self.seen_ms = self.seen_ms.max(now);
        if now.saturating_sub(self.wait_start_ms) < self.deadline_ms {
            return Next::Wait;
        }
        if self.phase == Phase::RollingBack {
            return self.finish(Phase::RollbackFailed, Reason::RollbackTimeout);
        }
        if self.read_ok && !self.ever_matched {
            return self.finish(Phase::NotApplied, Reason::Ignored);
        }
        self.expire(Reason::Timeout)
    }

    /// 用户点「立即退回」：自动退回关着也照做（这是用户的写）。
    pub fn revert(&mut self) -> Result<Next, &'static str> {
        match self.phase {
            Phase::Verifying if self.intent.is_none() => {
                Ok(self.start_rollback(Reason::UserRevert))
            }
            Phase::RollingBack => Err("already rolling back"),
            p if p.is_final() => Err("change already finished"),
            _ => Err("change is still being applied"),
        }
    }

    /// 用户点「保留现状」：取消退回，算用户确认。
    pub fn keep(&mut self) -> Result<(), &'static str> {
        match self.phase {
            Phase::Verifying if self.intent.is_none() => {
                self.finish(Phase::Confirmed, Reason::UserKeep);
                Ok(())
            }
            Phase::RollingBack => Err("already rolling back"),
            p if p.is_final() => Err("change already finished"),
            _ => Err("change is still being applied"),
        }
    }

    /// 被打断：同一项的覆盖写（superseded）、安全类写（preempted）、触屏接管（takeover）。
    pub fn cancel(&mut self, why: Reason) {
        if !self.phase.is_final() {
            self.finish(Phase::Cancelled, why);
        }
    }

    /// datad 启动时接着跑落盘的事务。`boot_id` 变了 = 整机重启（D13）：
    /// 第 2 次开机仍未确认、或几次开机累计等待已超过时限，马上退回（reboot_loop）；否则时限从现在重新计。
    /// 意图没清的不重做（D31）：applying 当作已写、按读回判断；rolling_back 当作退回已发过，绝不发第二次。
    pub fn resume(&mut self, boot_id: &str, now: u64) -> Next {
        if self.phase.is_final() {
            return Next::Done;
        }
        let rebooted = boot_id != self.boot_id;
        if rebooted {
            if self.waiting() && self.intent.is_none() {
                self.elapsed_prev_ms += self.seen_ms.saturating_sub(self.wait_start_ms);
            }
            self.boots += 1;
            self.boot_id = boot_id.to_owned();
        }
        match (self.phase, self.intent) {
            (Phase::Accepted, _) => {
                // 还没落意图就停了：设备没被动过。
                return self.finish(Phase::NotApplied, Reason::Ignored);
            }
            (Phase::Applying, _) | (Phase::Verifying, Some(Intent::Apply)) => {
                self.intent = None;
                self.phase = Phase::Verifying;
                self.wait_start_ms = now;
            }
            (Phase::RollingBack, Some(_)) => {
                self.intent = None;
                self.wait_start_ms = now;
            }
            _ if rebooted => self.wait_start_ms = now,
            _ => {}
        }
        self.seen_ms = now;
        self.generation += 1;
        if rebooted
            && self.phase == Phase::Verifying
            && (self.boots >= 2 || self.elapsed_prev_ms >= self.deadline_ms)
        {
            return self.expire(Reason::RebootLoop);
        }
        Next::Wait
    }

    /// 给客户端看的状态（`op.status`、提交和 busy 的回复里）。
    pub fn status(&self, now: u64) -> Value {
        let remaining = if self.waiting() {
            json!(
                self.deadline_ms
                    .saturating_sub(now.saturating_sub(self.wait_start_ms))
            )
        } else {
            Value::Null
        };
        json!({
            "op_id": self.op_id,
            "action": self.action,
            "item": self.item,
            "source": self.source,
            "undo": self.undo,
            "phase": self.phase,
            "reason": self.reason,
            "rollback_reason": self.rollback_reason,
            "target": self.target,
            "old": self.old,
            "rollback_to": self.rollback_to,
            "rollback_enabled": self.rollback_enabled,
            "age_ms": now.saturating_sub(self.created_ms),
            "remaining_ms": remaining,
        })
    }
}

#[cfg(test)]
mod tests;
