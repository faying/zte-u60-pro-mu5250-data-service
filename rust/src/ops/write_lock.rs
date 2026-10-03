//! 跨进程写锁（D29，write-op-layer.md E-O2）：`/var/run/u60-write.lock`（flock）。
//!
//! datad 每个写步骤、以及启动时处理 takeover/pending 的全过程都拿着它；触屏/agent 的应急直写脚本
//! （`u60-fallback.sh`）从判定 datad 不在到写完也拿着它。拿不到就等（每 50 ms 试一次）；
//! 等满 20 秒还拿不到就记一行、不拿锁照做（比卡住执行者让看门狗杀掉好；20 秒小于看门狗的 30 秒）。
//! `ZWRT_DATAD_WRITE_LOCK` 指定路径，空串 = 不用锁。文件打不开时只记一次日志。

use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(20);
const STEP: Duration = Duration::from_millis(50);

/// 拿着锁；丢掉就放。
pub struct WriteLock(Option<File>);

impl Drop for WriteLock {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            let _ = FileExt::unlock(f);
        }
    }
}

fn path() -> Option<String> {
    match std::env::var("ZWRT_DATAD_WRITE_LOCK") {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v),
        Err(_) => Some("/var/run/u60-write.lock".into()),
    }
}

static OPEN_FAILED: AtomicBool = AtomicBool::new(false);

pub async fn acquire() -> WriteLock {
    let Some(path) = path() else {
        return WriteLock(None);
    };
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            if !OPEN_FAILED.swap(true, Ordering::Relaxed) {
                eprintln!("ops: cannot open write lock {path}: {e}; writing without it");
            }
            return WriteLock(None);
        }
    };
    let start = tokio::time::Instant::now();
    loop {
        if file.try_lock_exclusive().is_ok() {
            return WriteLock(Some(file));
        }
        if start.elapsed() >= WAIT {
            eprintln!(
                "ops: write lock {path} still held after {} s, writing anyway",
                WAIT.as_secs()
            );
            return WriteLock(None);
        }
        tokio::time::sleep(STEP).await;
    }
}
