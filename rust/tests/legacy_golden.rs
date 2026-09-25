//! 旧接口 golden 对照（docs/STATE_V2.md V2-9，T1）：用本次编出来的 zwrt-datad 跑
//! tests/golden/golden.sh check。它对正常情形和每个 ubus 对象读失败的情形，
//! 把 /state 响应体和 /events 第一个事件块同 tests/golden/ 里的 golden 比较，
//! 只允许时间字段不同；/events 首条还必须是 `event: state` 且 data 等于 /state。
//! 需要 sh、curl、python3。

use std::{path::PathBuf, process::Command};

#[test]
fn legacy_events_golden_unchanged() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let script = root.join("tests/golden/golden.sh");
    let output = Command::new("sh")
        .arg(&script)
        .arg("check")
        .arg(env!("CARGO_BIN_EXE_zwrt-datad"))
        .output()
        .expect("run tests/golden/golden.sh");
    assert!(
        output.status.success(),
        "legacy /state or /events output changed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
