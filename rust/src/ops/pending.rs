//! 进行中事务的落盘（write-op-layer.md「重启续退」、D31）：`<目录>/pending.json`，原子写
//! （临时文件 → fsync → rename → fsync 目录）。另有触屏/agent 应急直写前留下的 `takeover` 标记（D18）。
//! 目录写不了时只记一行日志，datad 照常服务（不落盘就没有重启续退）。

use super::txn::Txn;
use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

const PENDING: &str = "pending.json";
const TAKEOVER: &str = "takeover";

pub struct Store {
    dir: Option<PathBuf>,
}

impl Store {
    /// `None` = 不落盘（测试或目录建不了）。
    pub fn open(dir: Option<PathBuf>) -> Self {
        let dir = dir.and_then(|d| match fs::create_dir_all(&d) {
            Ok(()) => Some(d),
            Err(e) => {
                eprintln!(
                    "ops: cannot use {}: {e}; changes will not survive a restart",
                    d.display()
                );
                None
            }
        });
        Self { dir }
    }

    pub fn save(&self, t: &Txn) {
        let Some(dir) = &self.dir else { return };
        if let Err(e) = write_atomic(
            dir,
            PENDING,
            &serde_json::to_vec(t).expect("txn serializes"),
        ) {
            eprintln!("ops: cannot save {PENDING}: {e}");
        }
    }

    pub fn clear(&self) {
        let Some(dir) = &self.dir else { return };
        match fs::remove_file(dir.join(PENDING)) {
            Ok(()) => sync_dir(dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("ops: cannot remove {PENDING}: {e}"),
        }
    }

    /// 读落盘的事务；读不懂的改名成 `pending.json.bad` 留着查，当作没有。
    pub fn load(&self) -> Option<Txn> {
        let dir = self.dir.as_ref()?;
        let path = dir.join(PENDING);
        let bytes = fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("ops: unreadable {PENDING} ({e}), set aside");
                let _ = fs::rename(&path, dir.join("pending.json.bad"));
                None
            }
        }
    }

    /// 有 `takeover` 标记就删掉并返回 true（D18：应急直写过，落盘的事务要放弃）。
    pub fn take_takeover(&self) -> bool {
        let Some(dir) = &self.dir else { return false };
        match fs::remove_file(dir.join(TAKEOVER)) {
            Ok(()) => {
                sync_dir(dir);
                true
            }
            Err(_) => false,
        }
    }
}

pub(super) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir);
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}
