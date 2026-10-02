//! 蜂窝口最近 30 秒发了多少包、收了多少包，给 `/v2/screen` 判「连上了但不通」（stall）。
//!
//! 设计在 manager `docs/designs/slow-diagnosis.md` §4.1、§13（工程复核 D3/D5）：
//! 时间窗记在采样循环里（每轮 `state::collect` 记一次），算好了当作普通输入交给
//! `screen::story()`，`story()` 仍是「给什么数据出什么结论」。门槛（≥ 20 包发、0 包收）
//! 在 screen.rs，这里只数。
//!
//! 计数来源：`rmnet_data0` 的 `statistics/{rx,tx}_packets`（和 /proc/net/dev 同一个计数）。
//! **未核对（ER1）**：IPA 硬件转发下，手机等客户端的流量可能不计入这张网卡。核对前这版
//! datad 不上机；核对不过就只改 `read()` 一处，换成厂商 `real_rx_bytes/real_tx_bytes`
//! （`zwrt_data get_wwandst`）或加上 `rmnet_ipa0`。

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The window the screen judges: at least this long between baseline and latest.
pub const SPAN_MS: u64 = 30_000;
/// Longer than this between two samples = the rounds stopped; the window says nothing.
const GAP_MS: u64 = 60_000;
const CELL_IF: &str = "rmnet_data0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    rx: u64,
    tx: u64,
    at_ms: u64,
}

/// Packets over the last ≥ 30 s on the cellular interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellWindow {
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub span_ms: u64,
}

#[derive(Default)]
struct Ring(VecDeque<Sample>);

impl Ring {
    /// One round's reading. A counter going backwards (interface recreated on
    /// redial) starts over; samples older than the 30 s baseline are dropped.
    fn push(&mut self, s: Sample) {
        if self
            .0
            .back()
            .is_some_and(|last| s.rx < last.rx || s.tx < last.tx || s.at_ms < last.at_ms)
        {
            self.0.clear();
        }
        self.0.push_back(s);
        let cut = s.at_ms.saturating_sub(SPAN_MS);
        // keep the newest sample that is ≥ 30 s old as the baseline
        while self.0.len() >= 2 && self.0[1].at_ms <= cut {
            self.0.pop_front();
        }
    }

    /// No reading this round: whatever came before can't be compared any more.
    fn lose(&mut self) {
        self.0.clear();
    }

    fn window(&self) -> Option<CellWindow> {
        let (first, last) = (self.0.front()?, self.0.back()?);
        let span = last.at_ms - first.at_ms;
        if span < SPAN_MS {
            return None;
        }
        if self
            .0
            .iter()
            .zip(self.0.iter().skip(1))
            .any(|(a, b)| b.at_ms - a.at_ms > GAP_MS)
        {
            return None;
        }
        Some(CellWindow {
            tx_packets: last.tx - first.tx,
            rx_packets: last.rx - first.rx,
            span_ms: span,
        })
    }
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> std::sync::MutexGuard<'static, Ring> {
    RING.get_or_init(|| Mutex::new(Ring::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The one place the counter is read (see the module note on ER1).
fn read() -> Option<(u64, u64)> {
    let root =
        std::env::var("ZWRT_DATAD_NET_CLASS_ROOT").unwrap_or_else(|_| "/sys/class/net".into());
    let path = Path::new(&root).join(CELL_IF).join("statistics");
    let n = |f: &str| -> Option<u64> { fs::read_to_string(path.join(f)).ok()?.trim().parse().ok() };
    Some((n("rx_packets")?, n("tx_packets")?))
}

/// Called once per sampling round.
pub fn sample(now_ms: u64) {
    match read() {
        Some((rx, tx)) => ring().push(Sample {
            rx,
            tx,
            at_ms: now_ms,
        }),
        None => ring().lose(),
    }
}

/// The current window for `/v2/screen`; `None` until 30 s of readings exist.
pub fn current() -> Option<CellWindow> {
    ring().window()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Ring, pts: &[(u64, u64, u64)]) {
        for &(at_ms, tx, rx) in pts {
            r.push(Sample { rx, tx, at_ms });
        }
    }

    #[test]
    fn needs_thirty_seconds() {
        let mut r = Ring::default();
        feed(&mut r, &[(0, 0, 0), (5_000, 5, 0), (25_000, 25, 0)]);
        assert_eq!(r.window(), None);
        feed(&mut r, &[(30_000, 30, 0)]);
        assert_eq!(
            r.window(),
            Some(CellWindow {
                tx_packets: 30,
                rx_packets: 0,
                span_ms: 30_000
            })
        );
    }

    #[test]
    fn baseline_is_the_newest_sample_thirty_seconds_back() {
        let mut r = Ring::default();
        let pts: Vec<_> = (0..=20).map(|k| (k * 5_000, k * 10, k)).collect();
        feed(&mut r, &pts);
        // latest at 100 s; baseline at 70 s
        assert_eq!(
            r.window(),
            Some(CellWindow {
                tx_packets: 60,
                rx_packets: 6,
                span_ms: 30_000
            })
        );
        assert!(r.0.len() <= 8);
    }

    #[test]
    fn one_second_rounds_keep_a_thirty_second_span() {
        let mut r = Ring::default();
        let pts: Vec<_> = (0..=120).map(|k| (k * 1_000 + 300, k, 0)).collect();
        feed(&mut r, &pts);
        let w = r.window().unwrap();
        assert_eq!((w.span_ms, w.tx_packets), (30_000, 30));
    }

    #[test]
    fn counter_going_back_starts_over() {
        let mut r = Ring::default();
        feed(&mut r, &[(0, 100, 50), (30_000, 200, 50), (35_000, 3, 0)]);
        assert_eq!(r.window(), None);
        feed(&mut r, &[(65_000, 40, 0)]);
        assert_eq!(r.window().unwrap().tx_packets, 37);
    }

    #[test]
    fn a_long_gap_says_nothing() {
        let mut r = Ring::default();
        feed(&mut r, &[(0, 0, 0), (61_000, 30, 0)]);
        assert_eq!(r.window(), None);
        feed(&mut r, &[(62_000, 31, 0)]);
        assert_eq!(r.window(), None);
        feed(&mut r, &[(91_000, 50, 0)]);
        assert_eq!(r.window().unwrap().tx_packets, 20);
    }

    #[test]
    fn a_missing_reading_starts_over() {
        let mut r = Ring::default();
        feed(&mut r, &[(0, 0, 0), (30_000, 30, 0)]);
        r.lose();
        assert_eq!(r.window(), None);
    }
}
