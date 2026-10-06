//! 数据接口 golden 对照（docs/STATE_V2.md V2-9，T1）：用本次编出来的 zwrt-datad 跑
//! tests/golden/golden.sh check。它对正常情形和每个 ubus 对象读失败的情形，
//! 把 /v2/state 响应体和 /v2/events 第一个事件块同 tests/golden/ 里的 golden 比较，
//! 只允许时间和序号字段不同；/v2/events 首条还必须是 `event: snapshot` 且 data 等于 /v2/state。
//! （旧 /state、/events 2026-10 删了，golden 改录 /v2。）
//! 需要 sh、curl、python3。

use std::{path::PathBuf, process::Command};

#[test]
fn v2_state_golden_unchanged() {
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
        "/v2/state or /v2/events output changed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 旧 /control 回复 golden（E4 T1，write-op-layer.md D26）：tests/golden/control_golden.sh check。
/// 按 control_cases.txt 发每个动作的成功、参数不对、ubus 失败请求，状态码和回复体逐字节比；
/// 再跑 control_contract.py：挂起到做完才回复、队列满 503 的回复体、state.set_interval 生效。
/// 需要 sh、curl、python3、openssl。
#[test]
fn legacy_control_golden_unchanged() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let script = root.join("tests/golden/control_golden.sh");
    let output = Command::new("sh")
        .arg(&script)
        .arg("check")
        .arg(env!("CARGO_BIN_EXE_zwrt-datad"))
        .output()
        .expect("run tests/golden/control_golden.sh");
    assert!(
        output.status.success(),
        "legacy /control replies changed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
