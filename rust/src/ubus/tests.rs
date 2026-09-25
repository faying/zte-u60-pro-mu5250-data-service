//! T3 测试：blob 编解码（手写字节对照 + 往返）、客户端对 mock ubusd（迟到回复、换 ID、ubusd 重启、
//! 超时重连与本轮跳过）、后端选择与 CLI 后端。

use super::backend::{BackendKind, CliBackend, SocketBackend, UbusBackend};
use super::blob::{self, Attr, MsgHdr, attr, blobmsg_type, msg_type, status};
use super::client::{RoundSkips, UbusClient, UbusError};
use super::mock_ubusd::{Action, Inject, MockUbusd};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_millis(250);

// ------------------------------------------------------------ 编解码

#[test]
fn blob_hello_frame_bytes() {
    let f = blob::encode_frame(MsgHdr::new(msg_type::HELLO, 0, 0x1234_5678), &[]);
    assert_eq!(f, [0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 0, 4]);
    let d = blob::decode_frame(&f).unwrap();
    assert_eq!(d.hdr, MsgHdr::new(msg_type::HELLO, 0, 0x1234_5678));
    assert!(d.body.is_empty());
}

#[test]
fn blob_lookup_request_bytes() {
    let mut body = Vec::new();
    blob::put_string(&mut body, attr::OBJPATH, "system");
    let f = blob::encode_frame(MsgHdr::new(msg_type::LOOKUP, 1, 0), &body);
    let want: &[u8] = &[
        0, 4, 0, 1, 0, 0, 0, 0, // ubus_msghdr: version 0, LOOKUP, seq 1, peer 0
        0, 0, 0, 16, // 顶层 blob：id 0，长度 16
        2, 0, 0, 11, b's', b'y', b's', b't', b'e', b'm', 0,
        0, // OBJPATH，长度 11 + 1 字节填充
    ];
    assert_eq!(f, want);
    let d = blob::decode_frame(&f).unwrap();
    let a = blob::msg_attrs(&d.body).unwrap();
    assert_eq!(
        blob::get_string(a[attr::OBJPATH as usize].as_ref().unwrap()),
        "system"
    );
}

/// {"s":"x","n":5,"b":true}（按这个顺序写入）的 blobmsg 字节，按 blobmsg.h 手算。
const TABLE_FIXTURE: &[u8] = &[
    0x83, 0, 0, 10, 0, 1, b's', 0, b'x', 0, 0,
    0, // STRING "s"="x"：头4 + 名字头4 + "x\0"，填 2
    0x85, 0, 0, 12, 0, 1, b'n', 0, 0, 0, 0, 5, // INT32 "n"=5
    0x87, 0, 0, 9, 0, 1, b'b', 0, 1, 0, 0, 0, // INT8(BOOL) "b"=true，填 3
];

#[test]
fn blobmsg_table_fixture_bytes() {
    let mut out = Vec::new();
    blob::blobmsg_put_json(&mut out, "s", &json!("x"));
    blob::blobmsg_put_json(&mut out, "n", &json!(5));
    blob::blobmsg_put_json(&mut out, "b", &json!(true));
    assert_eq!(out, TABLE_FIXTURE);
    let m = blob::blobmsg_object(TABLE_FIXTURE).unwrap();
    assert_eq!(Value::Object(m), json!({"s":"x","n":5,"b":true}));
}

#[test]
fn blobmsg_array_fixture_bytes() {
    // "l": ["x"]；数组元素 namelen=0，名字头 2+0+1 对齐到 4。
    let want: &[u8] = &[
        0x81, 0, 0, 20, 0, 1, b'l', 0, // ARRAY "l"，长度 20
        0x83, 0, 0, 10, 0, 0, 0, 0, b'x', 0, 0, 0, // STRING 元素
    ];
    let mut out = Vec::new();
    blob::blobmsg_put_json(&mut out, "l", &json!(["x"]));
    assert_eq!(out, want);
    assert_eq!(
        Value::Object(blob::blobmsg_object(want).unwrap()),
        json!({"l":["x"]})
    );
}

#[test]
fn blobmsg_roundtrip_all_types() {
    let v = json!({
        "str": "中文 ok", "empty": "", "i32": -7, "big": 1_i64 << 40, "neg64": -(1_i64 << 40),
        "t": true, "f": false, "d": 1.5, "null": null,
        "arr": [1, "two", [3], {"four": 4}], "tbl": {"x": {"y": []}}, "e": {}
    });
    let bytes = blob::blobmsg_table(v.as_object().unwrap());
    assert_eq!(Value::Object(blob::blobmsg_object(&bytes).unwrap()), v);
    // 整数按 CLI 规则：i32 范围内 INT32，否则 INT64；小数 DOUBLE。
    let attrs = blob::parse_attrs(&bytes).unwrap();
    let ty = |name: &str| {
        attrs
            .iter()
            .find(|a| blob::blobmsg_decode(a).unwrap().0 == name)
            .unwrap()
            .id
    };
    assert_eq!(ty("i32"), blobmsg_type::INT32);
    assert_eq!(ty("big"), blobmsg_type::INT64);
    assert_eq!(ty("d"), blobmsg_type::DOUBLE);
    assert_eq!(ty("t"), blobmsg_type::BOOL);
    assert_eq!(ty("null"), blobmsg_type::UNSPEC);
}

#[test]
fn blobmsg_decodes_int16_and_rejects_bad_input() {
    let mut out = Vec::new();
    blob::blobmsg_put_raw(&mut out, blobmsg_type::INT16, "h", &(-2_i16).to_be_bytes());
    assert_eq!(
        Value::Object(blob::blobmsg_object(&out).unwrap()),
        json!({"h":-2})
    );
    // 没有扩展位的属性不是 blobmsg。
    let mut plain = Vec::new();
    blob::put_attr(
        &mut plain,
        blobmsg_type::STRING,
        false,
        &[0, 0, 0, 0, b'x', 0],
    );
    assert!(blob::blobmsg_object(&plain).is_err());
    // INT32 长度不对。
    let mut bad = Vec::new();
    blob::blobmsg_put_raw(&mut bad, blobmsg_type::INT32, "n", &[0, 1]);
    assert!(blob::blobmsg_object(&bad).is_err());
    // 属性长度超出容器。
    assert!(blob::parse_attrs(&[0x83, 0, 0, 40, 0, 0]).is_err());
    let a = Attr {
        id: 0,
        extended: false,
        data: &[1, 2],
    };
    assert!(blob::get_u32(&a).is_err());
}

#[test]
fn frame_truncated_and_oversized_are_errors() {
    let mut body = Vec::new();
    blob::put_string(&mut body, attr::OBJPATH, "system");
    let f = blob::encode_frame(MsgHdr::new(msg_type::LOOKUP, 1, 0), &body);
    for n in 0..f.len() {
        assert!(blob::decode_frame(&f[..n]).is_err(), "truncated at {n}");
    }
    let mut big = [0u8; 12];
    let len = (blob::UBUS_MAX_MSGLEN + 4) as u32;
    big[8..].copy_from_slice(&len.to_be_bytes());
    assert!(blob::decode_head(&big).is_err());
    big[8..].copy_from_slice(&3_u32.to_be_bytes());
    assert!(blob::decode_head(&big).is_err(), "shorter than blob header");
}

#[tokio::test]
async fn read_frame_reports_eof_and_oversize() {
    let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
    use tokio::io::AsyncWriteExt;
    let mut head = vec![0, msg_type::DATA, 0, 1, 0, 0, 0, 0];
    head.extend(((blob::UBUS_MAX_MSGLEN + 8) as u32).to_be_bytes());
    a.write_all(&head).await.unwrap();
    assert!(matches!(
        blob::read_frame(&mut b).await,
        Err(blob::ReadError::Protocol(_))
    ));
    drop(a);
    assert!(matches!(
        blob::read_frame(&mut b).await,
        Err(blob::ReadError::Io(_))
    ));
}

// ------------------------------------------------------------ 客户端

fn client(m: &MockUbusd) -> UbusClient {
    UbusClient::with_timeout(m.path(), T)
}

#[tokio::test]
async fn ubus_call_lookup_then_invoke_caches_id() {
    let m = MockUbusd::start_new().await;
    m.add_method("system", 0x100, "board", json!({"board_name":"mu5250"}));
    m.add_method("system", 0x100, "info", json!({"uptime":5}));
    let mut c = client(&m);
    assert_eq!(
        c.call("system", "board", &json!({})).await.unwrap(),
        Some(json!({"board_name":"mu5250"}))
    );
    assert_eq!(
        c.call("system", "info", &json!({})).await.unwrap(),
        Some(json!({"uptime":5}))
    );
    assert_eq!(c.cached_id("system"), Some(0x100));
    let s = c.stats();
    assert_eq!(
        (s.connects, s.lookups, s.invokes, s.dropped_frames),
        (1, 1, 2, 0)
    );
    assert_eq!(
        (m.connections(), m.lookups(), m.invokes("system")),
        (1, 1, 2)
    );
}

#[tokio::test]
async fn ubus_args_are_encoded_like_the_cli() {
    let m = MockUbusd::start_new().await;
    m.add_method("svc", 7, "echo", json!({}));
    m.script("svc", "echo", Action::Echo);
    let mut c = client(&m);
    let args = json!({"a":"b","n":-3,"big":5_000_000_000_i64,"t":true,"l":[1,{"x":null}],"d":0.25});
    assert_eq!(c.call("svc", "echo", &args).await.unwrap(), Some(args));
}

#[tokio::test]
async fn ubus_late_reply_is_dropped() {
    let m = MockUbusd::start_new().await;
    m.add_method("svc.a", 0x200, "get", json!({"who":"A"}));
    m.script("svc.a", "get", Action::Late);
    let mut c = client(&m);

    // 请求 A：mock 扣住回复，客户端超时、关连接。
    let e = c.call("svc.a", "get", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::Timeout { .. }), "{e:?}");
    assert!(!c.is_connected());

    // 请求 B 在途时：先到 A 的迟到回复（seq=A、peer 对），再到一组 seq=B 但 peer 错的帧，最后才是 B 的回复。
    m.add_method("svc.a", 0x200, "get", json!({"who":"B"}));
    m.inject_before_next_invoke(Inject::WrongPeer(json!({"who":"wrong-peer"})));
    let before = c.stats().dropped_frames;
    let v = c.call("svc.a", "get", &json!({})).await.unwrap();
    assert_eq!(
        v,
        Some(json!({"who":"B"})),
        "B must get its own reply, not A's late one"
    );
    assert_eq!(
        c.stats().dropped_frames - before,
        4,
        "late DATA+STATUS and wrong-peer DATA+STATUS"
    );
    assert_eq!(m.connections(), 2);

    // 之后这条连接上没有残留帧：下一次调用照常。
    m.add_method("svc.a", 0x200, "get", json!({"who":"C"}));
    assert_eq!(
        c.call("svc.a", "get", &json!({})).await.unwrap(),
        Some(json!({"who":"C"}))
    );
    assert_eq!(c.stats().dropped_frames - before, 4);
}

#[tokio::test]
async fn ubus_foreign_seq_and_unrelated_frames_are_dropped() {
    let m = MockUbusd::start_new().await;
    m.add_method("svc", 9, "get", json!({"mine":1}));
    let mut c = client(&m);
    m.inject_before_next_invoke(Inject::OtherSeq(json!({"other":1})));
    m.inject_before_next_invoke(Inject::Unrelated);
    m.inject_before_next_invoke(Inject::WrongPeer(json!({"peer":1})));
    assert_eq!(
        c.call("svc", "get", &json!({})).await.unwrap(),
        Some(json!({"mine":1}))
    );
    assert_eq!(c.stats().dropped_frames, 5);
}

#[tokio::test]
async fn ubus_timeout_reconnects_and_skips_object() {
    let m = MockUbusd::start_new().await;
    m.add_method("svc.slow", 1, "get", json!({"slow":true}));
    m.add_method("svc.fast", 2, "get", json!({"fast":true}));
    m.script("svc.slow", "get", Action::Hang);
    let mut c = client(&m);
    c.call("svc.fast", "get", &json!({})).await.unwrap(); // 缓存 svc.fast 的 ID
    assert_eq!(m.connections(), 1);

    let mut round = RoundSkips::new();
    let t0 = Instant::now();
    let e = c
        .call_in_round(&mut round, "svc.slow", "get", &json!({}))
        .await
        .unwrap_err();
    let took = t0.elapsed();
    assert!(
        matches!(&e, UbusError::Timeout { object, .. } if object == "svc.slow"),
        "{e:?}"
    );
    assert!(e.is_timeout());
    assert!(
        took >= T - Duration::from_millis(20) && took < T * 4,
        "took {took:?}"
    );
    assert!(!c.is_connected(), "timeout closes the connection");
    assert_eq!(
        c.cached_id("svc.slow"),
        None,
        "timeout invalidates the object's ID"
    );
    assert_eq!(
        c.cached_id("svc.fast"),
        Some(2),
        "other LOOKUP results are kept"
    );
    assert_eq!(c.stats().timeouts, 1);
    assert!(round.is_skipped("svc.slow"));

    // 同一轮再读 svc.slow：直接 Skipped，不发请求。
    let invokes = m.invokes("svc.slow");
    let e = c
        .call_in_round(&mut round, "svc.slow", "get", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(e, UbusError::Skipped { .. }), "{e:?}");
    assert_eq!(m.invokes("svc.slow"), invokes);

    // 同一轮读别的对象：重新连（新 HELLO），不重新 LOOKUP。
    let lookups = c.stats().lookups;
    let v = c
        .call_in_round(&mut round, "svc.fast", "get", &json!({}))
        .await
        .unwrap();
    assert_eq!(v, Some(json!({"fast":true})));
    assert_eq!(m.connections(), 2);
    assert_eq!(c.stats().connects, 2);
    assert_eq!(c.stats().lookups, lookups);

    // 下一轮：svc.slow 重新 LOOKUP 后照常读。
    round.clear();
    let mock_lookups = m.lookups();
    let v = c
        .call_in_round(&mut round, "svc.slow", "get", &json!({}))
        .await
        .unwrap();
    assert_eq!(v, Some(json!({"slow":true})));
    assert_eq!(c.stats().lookups, lookups + 1);
    assert_eq!(m.lookups(), mock_lookups + 1);
}

#[tokio::test]
async fn ubus_object_id_change_relookups() {
    let m = MockUbusd::start_new().await;
    m.add_method("zwrt_bsp.battery", 10, "list", json!({"cap":50}));
    let mut c = client(&m);
    assert_eq!(
        c.call("zwrt_bsp.battery", "list", &json!({}))
            .await
            .unwrap(),
        Some(json!({"cap":50}))
    );
    // 服务重启，用新 ID 重新注册。
    m.change_id("zwrt_bsp.battery", 11);
    assert_eq!(
        c.call("zwrt_bsp.battery", "list", &json!({}))
            .await
            .unwrap(),
        Some(json!({"cap":50}))
    );
    assert_eq!(c.cached_id("zwrt_bsp.battery"), Some(11));
    let s = c.stats();
    assert_eq!((s.lookups, s.invokes, s.connects), (2, 3, 1));
    assert_eq!(m.invokes("?"), 1, "one invoke hit the stale ID");
}

#[tokio::test]
async fn ubus_ubusd_restart_reconnects() {
    let mut m = MockUbusd::start_new().await;
    m.add_method("system", 1, "info", json!({"n":1}));
    let mut c = client(&m);
    assert!(c.call("system", "info", &json!({})).await.is_ok());

    // ubusd 重启，对象用新 ID 重新注册：旧连接写失败说明请求没送到，客户端当场重连重发。
    m.restart().await;
    m.change_id("system", 2);
    assert_eq!(
        c.call("system", "info", &json!({})).await.unwrap(),
        Some(json!({"n":1}))
    );
    assert_eq!(m.connections(), 2);
    assert_eq!(c.stats().connects, 2);
    assert_eq!(c.cached_id("system"), Some(2));

    // ubusd 不在：报 Io，不挂住；回来后下一次调用成功。
    m.stop().await;
    let t0 = Instant::now();
    let e = c.call("system", "info", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::Io(_)), "{e:?}");
    assert!(t0.elapsed() < T);
    m.start();
    assert_eq!(
        c.call("system", "info", &json!({})).await.unwrap(),
        Some(json!({"n":1}))
    );
    assert_eq!(m.connections(), 3);
}

#[tokio::test]
async fn ubus_status_errors_and_no_data() {
    let m = MockUbusd::start_new().await;
    m.add_method("svc", 5, "get", json!({"v":1}));
    m.add_method("svc", 5, "set", json!({}));
    let mut c = client(&m);

    let e = c.call("nope", "get", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::NotFound { .. }), "{e:?}");

    let e = c.call("svc", "missing", &json!({})).await.unwrap_err();
    assert!(
        matches!(e, UbusError::Status { code, .. } if code == status::METHOD_NOT_FOUND),
        "{e:?}"
    );
    assert_eq!(
        c.cached_id("svc"),
        None,
        "METHOD_NOT_FOUND invalidates the cached ID"
    );

    m.script("svc", "get", Action::Status(status::PERMISSION_DENIED));
    let e = c.call("svc", "get", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::Status { code: 6, .. }), "{e:?}");
    assert_eq!(c.cached_id("svc"), Some(5), "other statuses keep the cache");

    m.script("svc", "set", Action::NoData);
    assert_eq!(c.call("svc", "set", &json!({"x":1})).await.unwrap(), None);

    // 在超时之内的慢回复照常收到。
    m.script("svc", "get", Action::Delay(T / 3));
    assert_eq!(
        c.call("svc", "get", &json!({})).await.unwrap(),
        Some(json!({"v":1}))
    );

    // 对象消失：NOT_FOUND 后重新 LOOKUP 也找不到。
    m.remove_object("svc");
    let e = c.call("svc", "get", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::NotFound { .. }), "{e:?}");
    assert_eq!(c.stats().connects, 1);
    assert_eq!(c.stats().timeouts, 0);
}

#[tokio::test]
async fn ubus_invalid_arguments_send_nothing() {
    let m = MockUbusd::start_new().await;
    let mut c = client(&m);
    for (o, me, a) in [
        ("bad name", "x", json!({})),
        ("svc", "a;b", json!({})),
        ("svc", "x", json!([1])),
    ] {
        let e = c.call(o, me, &a).await.unwrap_err();
        assert!(matches!(e, UbusError::InvalidArgument(_)), "{e:?}");
    }
    assert_eq!(
        c.call("", "x", &json!({})).await.unwrap_err().to_string(),
        "invalid ubus name"
    );
    assert_eq!(m.connections(), 0);
}

// ------------------------------------------------------------ 后端

#[test]
fn backend_kind_parse() {
    assert_eq!(BackendKind::parse(None), (BackendKind::Cli, None));
    assert_eq!(BackendKind::parse(Some("")), (BackendKind::Cli, None));
    assert_eq!(BackendKind::parse(Some("cli")), (BackendKind::Cli, None));
    assert_eq!(
        BackendKind::parse(Some("socket")),
        (BackendKind::Socket, None)
    );
    assert_eq!(
        BackendKind::parse(Some(" socket\n")),
        (BackendKind::Socket, None)
    );
    let (k, w) = BackendKind::parse(Some("SOCKET"));
    assert_eq!(k, BackendKind::Cli);
    assert!(w.unwrap().contains("SOCKET"));
}

fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// 刚写好的脚本在多线程测试里 exec 偶尔会 ETXTBSY（别的线程 fork 时继承了写句柄），重试几次。
async fn cli_call(b: &mut CliBackend, o: &str, m: &str, a: &Value) -> Result<Value, UbusError> {
    for _ in 0..20 {
        let r = b.call(o, m, a).await;
        match &r {
            Err(UbusError::Io(msg)) if msg.contains("spawn") => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            _ => return r,
        }
    }
    b.call(o, m, a).await
}

#[tokio::test]
async fn cli_backend_matches_state_ubus() {
    let dir = std::env::temp_dir().join(format!("zwrt-ubus-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/mock_ubus.sh");
    let mut b = CliBackend::new(fixture, Duration::from_secs(8));
    assert_eq!(b.kind(), BackendKind::Cli);
    let v = cli_call(&mut b, "system", "board", &json!({}))
        .await
        .unwrap();
    assert!(v.is_object(), "{v}");

    let mut empty = CliBackend::new(script(&dir, "empty.sh", "exit 0"), Duration::from_secs(8));
    let e = cli_call(&mut empty, "svc", "set", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(e, UbusError::NoData { .. }), "{e:?}");
    let cli_empty = e.to_string();
    assert!(cli_empty.starts_with("invalid ubus JSON: "), "{cli_empty}");

    let mut fail = CliBackend::new(script(&dir, "fail.sh", "exit 4"), Duration::from_secs(8));
    let e = cli_call(&mut fail, "svc", "get", &json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, UbusError::Io(m) if m.contains("exited with")),
        "{e:?}"
    );

    let mut slow = CliBackend::new(
        script(&dir, "slow.sh", "sleep 5"),
        Duration::from_millis(100),
    );
    let e = cli_call(&mut slow, "svc", "get", &json!({}))
        .await
        .unwrap_err();
    assert!(e.is_timeout(), "{e:?}");
    assert!(e.to_string().ends_with("timed out"), "{e}");

    let e = cli_call(&mut b, "svc", "get", &json!("x"))
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "args must be an object");

    // Socket 后端在「OK 但没数据」时给出和 CLI 后端相同的错误文字。
    let m = MockUbusd::start_new().await;
    m.add_method("svc", 3, "set", json!({}));
    m.script("svc", "set", Action::NoData);
    let mut s = SocketBackend::new(client(&m));
    assert_eq!(s.kind(), BackendKind::Socket);
    let e = s.call("svc", "set", &json!({})).await.unwrap_err();
    assert!(matches!(e, UbusError::NoData { .. }), "{e:?}");
    assert_eq!(e.to_string(), cli_empty);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 执行者（T4）要把后端放进 `tokio::spawn` 的任务里：future 必须是 Send。
#[test]
fn backend_futures_are_send() {
    fn assert_send<F: std::future::Future + Send>(_: F) {}
    let mut b = super::backend::Backend::Cli(CliBackend::new("/bin/false", Duration::from_secs(1)));
    let args = json!({});
    assert_send(b.call("a", "b", &args));
    let mut c = UbusClient::new("/nonexistent");
    assert_send(c.call("a", "b", &args));
}
