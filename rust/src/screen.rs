//! `GET /v2/screen`：触屏首页信号卡和状态栏要显示的结论，从这一份 /state 算出来。
//!
//! 规则原来在触屏的 C 里（touch-ui `src/net_view.c`、`src/ui_logic.c`），2026-09-26
//! 起搬到这里，屏幕只负责画（manager `docs/screen-logic-move.md`）。为了保证搬的时候
//! 一条规则都没变，这里刻意照着 C 的读法：同样的字段长度截断、`atoi`/`atof`/`sscanf`
//! 的前缀解析、`(int)` 向零取整、`%.0f`/`%.1f` 的写法。`tests/fixtures/screen_net_corpus.jsonl`
//! 是 C 对 1700 多份 /state 算出的结果，测试要求这里逐字段一样
//! （生成方法见 touch-ui `tests/parity/gen.py`）。
//!
//! 不在这里的：大字换结论前的 15 秒稳定、「已 N 分钟」无服务计时——那是「这块屏
//! 显示过什么、什么时候」，留在屏幕上。

use serde::Serialize;
use serde_json::Value;

pub const SCREEN_VERSION: u32 = 1;
const CA_MAX: usize = 5;

// ---- C-compatible scalar reads (touch-ui json.c + libc) -------------------

/// What C's `json_get` copies for a value: strings unescaped, everything else
/// as its JSON text (`null` → "null", numbers as written). `None` = key absent.
fn raw(obj: &Value, key: &str) -> Option<String> {
    match obj.get(key)? {
        Value::String(s) => Some(s.clone()),
        v => Some(v.to_string()),
    }
}

/// `getstr` into `char dst[cap]`: missing → "", longer → cut to cap-1 bytes.
fn getstr(obj: &Value, key: &str, cap: usize) -> String {
    cstr(raw(obj, key).unwrap_or_default(), cap)
}

fn cstr(mut s: String, cap: usize) -> String {
    if s.len() > cap - 1 {
        let mut n = cap - 1;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// `strtol(s, &end, 10)`: leading space, sign, digits; `None` when no digit.
fn strtol(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut v: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add(i64::from(b[i] - b'0'));
        i += 1;
    }
    (i > start).then_some(if neg { -v } else { v })
}

fn atoi(s: &str) -> i64 {
    strtol(s).unwrap_or(0)
}

/// C `atoi(bs + 1)` on a band label ("n78" → 78, "-" → 0).
fn atoi_after_first_byte(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0;
    }
    i64::from(atoi(&String::from_utf8_lossy(&b[1..])) as i32)
}

/// `strtod` prefix: returns the value and the bytes consumed (0 = none).
fn strtod(s: &str) -> (f64, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let num_start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return (0.0, 0);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    (s[num_start..i].parse().unwrap_or(0.0), i)
}

fn atof(s: &str) -> f64 {
    strtod(s).0
}

/// `json_get_int(obj, key, def)`.
fn get_int(obj: &Value, key: &str, def: i64) -> i64 {
    raw(obj, key).and_then(|s| strtol(&s)).unwrap_or(def)
}

/// C `(int)x` for the doubles we see (truncation toward zero).
fn to_int(x: f64) -> i64 {
    x.trunc() as i64
}

/// `sscanf(rec, "%lf,%lf,…")` with `n` conversions: how many succeeded.
fn scan_floats(rec: &str, n: usize, out: &mut [f64]) -> usize {
    let mut rest = rec;
    for (k, slot) in out.iter_mut().enumerate().take(n) {
        let (v, used) = strtod(rest);
        if used == 0 {
            return k;
        }
        *slot = v;
        rest = &rest[used..];
        if k + 1 < n {
            match rest.strip_prefix(',') {
                Some(r) => rest = r,
                None => return k + 1,
            }
        }
    }
    n
}

fn upper_prefix(s: &str, n: usize) -> String {
    let b: Vec<u8> = s.bytes().take(n).map(|c| c.to_ascii_uppercase()).collect();
    String::from_utf8_lossy(&b).into_owned()
}

// ---- the screen's parse of /state (data.c parse_snapshot, net part) --------

#[derive(Default, Debug, Clone)]
struct Data {
    net_type: String,
    operator_name: String,
    roaming: String,
    band: String,
    nr_band: String,
    nr_snr: String,
    wan_status: String,
    lte_snr: String,
    bandwidth: String,
    operate_mode: String,
    lte_pci: i64,
    channel: i64,
    nr_bw: String,
    nrca: String,
    lteca: String,
    net_select: String,
    bars: i64,
    nr_rsrp: i64,
    nr_rsrq: i64,
    lte_rsrp: i64,
    lte_rsrq: i64,
    mcc: i64,
    mnc: i64,
    nr_pci: i64,
    nr_channel: i64,
    sim_state: String,
    sim_imsi: String,
    sim_spn: String,
    ambr_dl: f64,
    rx_speed: i64,
    cell_data: i64,
    cell_roam: i64,
}

fn mainland_operator_cn(mcc: i64, mnc: i64, raw: &str) -> Option<&'static str> {
    if mcc == 460 {
        match mnc {
            0 | 2 | 4 | 7 | 8 => return Some("中国移动"),
            1 | 6 | 9 => return Some("中国联通"),
            3 | 5 | 11 => return Some("中国电信"),
            15 => return Some("中国广电"),
            _ => {}
        }
    }
    match raw {
        "China Mobile" | "CMCC" => Some("中国移动"),
        "China Unicom" | "CUCC" => Some("中国联通"),
        "China Telecom" | "CTCC" => Some("中国电信"),
        "China Broadnet" | "China Broadcasting Network" => Some("中国广电"),
        _ => None,
    }
}

fn parse(state: &Value) -> Data {
    let mut d = Data { cell_data: -1, cell_roam: -1, ..Default::default() };
    if let Some(net) = state.get("net") {
        d.net_type = getstr(net, "type", 16);
        d.operator_name = getstr(net, "operator", 48);
        d.roaming = getstr(net, "roaming", 16);
        d.band = getstr(net, "band", 16);
        d.nr_band = getstr(net, "nr_band", 16);
        d.nr_snr = getstr(net, "nr_snr", 12);
        d.wan_status = getstr(net, "wan_status", 32);
        d.lte_snr = getstr(net, "lte_snr", 12);
        d.bandwidth = getstr(net, "bandwidth", 12);
        d.operate_mode = getstr(net, "operate_mode", 16);
        d.lte_pci = i64::from(get_int(net, "lte_pci", 0) as i32);
        d.channel = get_int(net, "channel", 0);
        d.nr_bw = getstr(net, "nr_bw", 12);
        d.nrca = getstr(net, "nrca", 256);
        d.lteca = getstr(net, "lteca", 256);
        d.net_select = getstr(net, "net_select", 16);
        d.bars = i64::from(get_int(net, "bars", 0) as i32);
        d.nr_rsrp = i64::from(get_int(net, "nr_rsrp", 0) as i32);
        d.nr_rsrq = i64::from(get_int(net, "nr_rsrq", 0) as i32);
        d.lte_rsrp = i64::from(get_int(net, "lte_rsrp", 0) as i32);
        d.lte_rsrq = i64::from(get_int(net, "lte_rsrq", 0) as i32);
        d.mcc = i64::from(get_int(net, "mcc", 0) as i32);
        d.mnc = i64::from(get_int(net, "mnc", 0) as i32);
        d.nr_pci = i64::from(get_int(net, "nr_pci", 0) as i32);
        d.nr_channel = get_int(net, "nr_channel", 0);
        if d.channel == 0 {
            d.channel = get_int(net, "lte_channel", 0);
        }
        if let Some(cn) = mainland_operator_cn(d.mcc, d.mnc, &d.operator_name) {
            d.operator_name = cstr(cn.to_string(), 48);
        }
    }
    if let Some(cell) = state.get("interfaces").and_then(|i| i.get("cellular")) {
        let v = get_int(cell, "enable", -1);
        d.cell_data = if v == 0 || v == 1 { v } else { -1 };
        let v = get_int(cell, "roam_enable", -1);
        d.cell_roam = if v == 0 || v == 1 { v } else { -1 };
    }
    if let Some(t) = state.get("traffic") {
        d.rx_speed = get_int(t, "rx_speed", 0);
    }
    if let Some(q) = state.get("qos") {
        d.ambr_dl = raw(q, "ambr_dl").map(|s| atof(&cstr(s, 32))).unwrap_or(0.0);
    }
    if let Some(sim) = state.get("sim") {
        d.sim_state = getstr(sim, "state", 24);
        d.sim_imsi = getstr(sim, "imsi", 20);
        d.sim_spn = getstr(sim, "spn", 32);
    }
    d
}

// ---- ui_logic.c ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rat {
    None,
    G2,
    G3,
    G4,
    Nsa,
    Sa,
}

fn has(s: &str, w: &str) -> bool {
    s.contains(w)
}

fn has_ci(s: &str, w: &str) -> bool {
    upper_prefix(s, 47).contains(w)
}

fn has5a(s: &str) -> bool {
    has_ci(s, "5G-A") || has_ci(s, "5GA") || has_ci(s, "5G_A") || has_ci(s, "5G-ADV")
}

fn rat_of(raw: &str) -> Rat {
    if raw.is_empty() {
        return Rat::None;
    }
    let u = upper_prefix(raw, 31);
    let u = u.as_str();
    if has(u, "LIMIT") || has(u, "EMERGENCY") || has(u, "NO_SERVICE") || has(u, "NOSERVICE") {
        return Rat::None;
    }
    if has(u, "NSA") || has(u, "ENDC") || has(u, "EN-DC") {
        return Rat::Nsa;
    }
    if u == "SA"
        || u == "NR"
        || has(u, "5G")
        || has(u, "NR5G")
        || u.starts_with("SA_")
        || u.starts_with("NR_")
        || has(u, "_SA")
        || has(u, " SA")
    {
        return Rat::Sa;
    }
    if has(u, "LTE") || has(u, "4G") {
        return Rat::G4;
    }
    if ["WCDMA", "UMTS", "HSPA", "HSDPA", "HSUPA", "TD-SCDMA", "TDSCDMA", "CDMA2000", "EVDO", "EV-DO", "EHRPD", "HRPD", "3G"]
        .iter()
        .any(|w| has(u, w))
    {
        return Rat::G3;
    }
    if ["GSM", "GPRS", "EDGE", "2G", "CDMA", "1XRTT", "1X"].iter().any(|w| has(u, w)) {
        return Rat::G2;
    }
    Rat::None
}

fn rat_family(raw: &str) -> &'static str {
    if raw.is_empty() {
        return "";
    }
    if has_ci(raw, "LTE") || has_ci(raw, "4G") {
        return "LTE";
    }
    if matches!(rat_of(raw), Rat::Sa | Rat::Nsa) {
        return "NR";
    }
    if has_ci(raw, "TD-SCDMA") || has_ci(raw, "TDSCDMA") {
        return "TD-SCDMA";
    }
    if has_ci(raw, "CDMA2000") || has_ci(raw, "EVDO") || has_ci(raw, "EV-DO") || has_ci(raw, "HRPD") {
        return "CDMA2000";
    }
    if has_ci(raw, "HSPA+") || has_ci(raw, "DC-HSPA") || has_ci(raw, "HSPAP") {
        return "HSPA+";
    }
    if has_ci(raw, "HSPA") || has_ci(raw, "HSDPA") || has_ci(raw, "HSUPA") {
        return "HSPA";
    }
    if has_ci(raw, "WCDMA") || has_ci(raw, "UMTS") {
        return "WCDMA";
    }
    if has_ci(raw, "EDGE") {
        return "EDGE";
    }
    if has_ci(raw, "GPRS") {
        return "GPRS";
    }
    if has_ci(raw, "GSM") {
        return "GSM";
    }
    if has_ci(raw, "CDMA") || has_ci(raw, "1X") {
        return "CDMA 1X";
    }
    ""
}

fn rat_long(raw: &str, lte_active: i64) -> String {
    let fam = rat_family(raw);
    let sp = if fam.is_empty() { "" } else { " " };
    let s = match rat_of(raw) {
        Rat::Sa => "5G SA".to_string(),
        Rat::Nsa => "5G NSA · 4G 锚点".to_string(),
        Rat::G4 => if lte_active >= 2 { "4G LTE-A" } else { "4G LTE" }.to_string(),
        Rat::G3 => format!("3G{sp}{fam}"),
        Rat::G2 => format!("2G{sp}{fam}"),
        Rat::None => String::new(),
    };
    cstr(s, 32)
}

/// `ui_band_short`: the last run of digits as n78 / B3; a frequency (≥ 450) or
/// no number → the raw text; empty → "-".
fn band_short(raw: &str, nr: bool) -> String {
    if raw.is_empty() {
        return "-".into();
    }
    let b = raw.as_bytes();
    let mut last = None;
    for i in 0..b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            last = Some(i);
        }
    }
    match last {
        Some(i) if atoi(&raw[i..]) < 450 => format!("{}{}", if nr { 'n' } else { 'B' }, atoi(&raw[i..])),
        _ => cstr(raw.to_string(), 16),
    }
}

fn bars_tier(bars: i64) -> i64 {
    if bars <= 0 {
        -1
    } else if bars >= 4 {
        2
    } else if bars == 3 {
        1
    } else {
        0
    }
}

fn imsi_plmn(imsi: &str) -> Option<(i64, i64)> {
    const MNC3: [i64; 24] = [
        302, 310, 311, 312, 313, 314, 315, 316, 334, 338, 342, 344, 346, 348, 354, 356, 358, 360, 365, 376, 405, 708,
        722, 732,
    ];
    let b = imsi.as_bytes();
    if b.len() < 6 || !b[..6].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let dg = |i: usize| i64::from(b[i] - b'0');
    let c = dg(0) * 100 + dg(1) * 10 + dg(2);
    let mut n = dg(3) * 10 + dg(4);
    if MNC3.contains(&c) {
        n = n * 10 + dg(5);
    }
    if c < 200 {
        return None;
    }
    Some((c, n))
}

fn operator_logo(mcc: i64, mnc: i64) -> Option<&'static str> {
    const K: &[(i64, i64, &str)] = &[
        (460, 0, "china-mobile"), (460, 2, "china-mobile"), (460, 4, "china-mobile"),
        (460, 7, "china-mobile"), (460, 8, "china-mobile"),
        (460, 1, "china-unicom"), (460, 6, "china-unicom"), (460, 9, "china-unicom"),
        (460, 3, "china-telecom"), (460, 5, "china-telecom"), (460, 11, "china-telecom"),
        (454, 0, "csl"), (454, 2, "csl"), (454, 10, "csl"), (454, 18, "csl"),
        (454, 3, "three-hk"), (454, 4, "three-hk"),
        (454, 6, "smartone"), (454, 15, "smartone"),
        (454, 12, "cmhk"), (454, 13, "cmhk"),
        (454, 7, "china-unicom"), (454, 16, "csl"), (454, 19, "csl"), (454, 20, "csl"),
        (454, 31, "china-telecom"),
        (455, 5, "three-hk"), (455, 7, "china-telecom"),
        (455, 1, "ctm"), (455, 4, "ctm"),
        (466, 92, "chunghwa"), (466, 1, "fetnet"), (466, 97, "taiwan-mobile"),
        (440, 10, "docomo"), (440, 20, "softbank"), (440, 11, "rakuten"),
        (440, 50, "au"), (440, 51, "au"), (440, 52, "au"), (440, 53, "au"), (440, 54, "au"),
        (450, 5, "skt"), (450, 8, "kt"), (450, 6, "lguplus"),
        (525, 1, "singtel"), (525, 3, "m1"), (525, 5, "starhub"),
        (310, 260, "t-mobile-us"), (310, 410, "att"), (311, 480, "verizon"),
    ];
    K.iter().find(|k| k.0 == mcc && k.1 == mnc).map(|k| k.2)
}

fn sim_logo(mcc: i64, mnc: i64, spn: &str) -> Option<&'static str> {
    let low: String = spn.bytes().take(31).map(|c| c.to_ascii_lowercase() as char).collect();
    if low.contains("cmlink") {
        return Some("cmlink");
    }
    operator_logo(mcc, mnc)
}

fn pinned_mode(sel: &str) -> Option<&'static str> {
    if sel.is_empty() || !has_ci(sel, "ONLY") {
        return None;
    }
    if sel == "Only_GSM_WCDMA" {
        return Some("只用 3G 和 2G");
    }
    if has_ci(sel, "GSM") || has_ci(sel, "2G") {
        return Some("只用 2G");
    }
    if has_ci(sel, "WCDMA") || has_ci(sel, "3G") || has_ci(sel, "TD") {
        return Some("只用 3G");
    }
    if has_ci(sel, "LTE") || has_ci(sel, "4G") {
        return Some("只用 4G");
    }
    if has_ci(sel, "5G") || has_ci(sel, "NR") {
        return Some("只用 5G");
    }
    Some("限定了制式")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Ok,
    Warn,
    Bad,
    Neutral,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    #[default]
    None,
    Limit,
    Weak,
    Noise,
    Crowd,
    Narrow,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sw {
    Unknown,
    On,
    Off,
}

struct NetIn<'a> {
    sim_state: &'a str,
    airplane: bool,
    net_type: &'a str,
    bars: i64,
    data_up: bool,
    roaming: i64,
    n_active: i64,
    nr_active: i64,
    lte_active: i64,
    mhz: i64,
    sinr_valid: bool,
    sinr: f64,
    rsrp_valid: bool,
    rsrp: i64,
    rsrq_valid: bool,
    rsrq: i64,
    mcc: i64,
    mnc: i64,
    nr_band: i64,
    nr_mhz: i64,
    rx_bps: i64,
    ambr_dl: f64,
    net_select: &'a str,
    data_sw: Sw,
    roam_sw: Sw,
}

/// `ui_net_story` output.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Story {
    pub tone: Tone,
    pub cause: Cause,
    pub sig_tone: Tone,
    pub noise_tone: Tone,
    /// 顺畅 / 慢：信号弱 / 无服务 …
    pub headline: String,
    /// What it means / what to do; "" when all is fine.
    pub hint: String,
    /// The status bar's phone-style label: 5G-A / 5G UC / 5G / 4G+ / 4G / 3G / 2G / SOS / 无服务.
    pub rat: String,
    pub link: String,
    /// 强 / 中 / 弱 from the bars.
    pub sig: String,
    /// 小 / 中 / 大 from SINR.
    pub noise: String,
    /// 正常 / 高 while downloading.
    pub load: String,
    /// 无 / 有 / — (AMBR unknown).
    pub limit: String,
}

fn net_label(raw: &str) -> &'static str {
    match rat_of(raw) {
        Rat::Sa | Rat::Nsa => {
            if has5a(raw) {
                "5G-A"
            } else {
                "5G"
            }
        }
        Rat::G4 => "4G",
        Rat::G3 => "3G",
        Rat::G2 => "2G",
        Rat::None => "",
    }
}

fn net_badge(i: &NetIn) -> String {
    let r = rat_of(i.net_type);
    let us = (310..=316).contains(&i.mcc);
    let mut out = net_label(i.net_type);
    if has_ci(i.net_type, "LIMIT") || has_ci(i.net_type, "EMERGENCY") {
        return "SOS".into();
    }
    if matches!(r, Rat::Sa | Rat::Nsa) {
        if i.mcc == 460 {
            let unicom = matches!(i.mnc, 1 | 6 | 9);
            if i.nr_active >= 3 || (unicom && i.nr_active >= 2 && i.nr_mhz >= 200) {
                out = "5G-A";
            }
        } else if i.mcc == 310 && i.mnc == 260 {
            if i.nr_band == 41 {
                out = "5G UC";
            }
        } else if i.mcc == 311 && i.mnc == 480 {
            if i.nr_band == 77 || i.nr_band == 48 {
                out = "5G UW";
            }
        } else if i.mcc == 310 && i.mnc == 410 && (i.nr_band == 77 || i.mhz >= 50) {
            out = "5G+";
        }
    } else if r == Rat::G4 {
        if i.lte_active >= 2 && (i.mcc == 466 || i.mcc == 440) {
            out = "4G+";
        } else if us {
            out = "LTE";
        }
    }
    out.into()
}

fn sim_usable(st: &str) -> bool {
    st.is_empty() || st.contains("ready")
}

fn story(i: &NetIn) -> Story {
    let rat = rat_of(i.net_type);
    let pin = pinned_mode(i.net_select);
    let limited = has_ci(i.net_type, "LIMIT") || has_ci(i.net_type, "EMERGENCY");
    let mut o = Story { rat: cstr(net_badge(i), 32), ..Default::default() };

    if matches!(rat, Rat::G2 | Rat::G3) {
        o.link = "这个制式没有载波聚合".into();
    } else if i.n_active > 0 {
        let w = if i.mhz >= 200 {
            "带宽很宽"
        } else if i.mhz >= 100 {
            "带宽充足"
        } else if i.mhz >= 40 {
            "带宽一般"
        } else if i.mhz > 0 {
            "带宽偏窄"
        } else {
            ""
        };
        let n = if rat == Rat::Nsa {
            format!("4G 锚点 + 5G，{} 条载波", i.n_active)
        } else if i.n_active > 1 {
            format!("{} 条载波聚合", i.n_active)
        } else {
            "单载波".to_string()
        };
        let n = cstr(n, 64);
        o.link = cstr(format!("{n}{}{w}", if w.is_empty() { "" } else { " · " }), 96);
    }

    let st = bars_tier(i.bars);
    if st >= 0 {
        o.sig = ["弱", "中", "强"][st as usize].into();
        o.sig_tone = match st {
            2 => Tone::Ok,
            1 => Tone::Warn,
            _ => Tone::Bad,
        };
    }
    let mut nq = -1;
    if i.sinr_valid {
        nq = if i.sinr >= 13.0 {
            2
        } else if i.sinr >= 0.0 {
            1
        } else {
            0
        };
        o.noise = ["大", "中", "小"][nq as usize].into();
        o.noise_tone = match nq {
            2 => Tone::Ok,
            1 => Tone::Warn,
            _ => Tone::Bad,
        };
    }
    let busy = i.rx_bps >= 125_000;
    let nr = matches!(rat, Rat::Sa | Rat::Nsa);
    let mut crowd = false;
    if busy && i.rsrp_valid && i.rsrp >= -100 && i.rsrq_valid {
        crowd = i.rsrq < if nr { -15 } else { -12 };
        o.load = if crowd { "高" } else { "正常" }.into();
    }
    let capped = i.ambr_dl > 0.0 && i.ambr_dl < 10.0;
    o.limit = if i.ambr_dl <= 0.0 {
        "—"
    } else if capped {
        "有"
    } else {
        "无"
    }
    .into();
    let narrow = i.n_active == 1 && i.mhz > 0 && i.mhz <= 20 && (rat == Rat::G4 || nr);
    let roam_note = if i.roaming == 1 { "；漫游中" } else { "" };

    let say = |mut o: Story, cause: Cause, tone: Tone, head: &str, hint: String| {
        o.cause = cause;
        o.tone = tone;
        o.headline = cstr(head.to_string(), 32);
        o.hint = cstr(hint, 128);
        o
    };
    if !sim_usable(i.sim_state) {
        return say(o, Cause::None, Tone::Bad, "无 SIM", "插卡，或在「功能 → eSIM」启用".into());
    }
    if i.airplane {
        return say(o, Cause::None, Tone::Neutral, "移动网络已关", "飞行模式开着，去管理网页关掉".into());
    }
    if limited {
        let h = if i.roaming == 1 {
            "没注册上：卡要开漫游，或换当地卡"
        } else {
            "没注册上：欠费、停机，或这里没这家的网"
        };
        return say(o, Cause::None, Tone::Bad, "只能紧急呼叫", h.into());
    }
    if i.bars <= 0 || rat == Rat::None {
        o.rat = "无服务".into();
        let h = if pin.is_some() {
            "正在搜网；制式被限定，去「锁频」改回自动"
        } else {
            "正在搜网，换个位置试试"
        };
        return say(o, Cause::None, Tone::Bad, "无服务", h.into());
    }
    if !i.data_up {
        if i.data_sw == Sw::Off {
            return say(o, Cause::None, Tone::Bad, "没连上网", "移动数据关着：去「蜂窝」打开".into());
        }
        if i.roaming == 1 && i.roam_sw == Sw::Off {
            return say(o, Cause::None, Tone::Bad, "没连上网", "数据漫游关着：去「蜂窝」打开，卡也要开通".into());
        }
        if i.roaming == 1 && i.roam_sw == Sw::On {
            return say(
                o,
                Cause::None,
                Tone::Bad,
                "没连上网",
                "正在拨号，换网后要半分钟左右；一直不通多半是卡在这家网络没开漫游".into(),
            );
        }
        let h = if i.roaming == 1 {
            "数据没拨上：看「蜂窝」里数据漫游开没开，卡也要开通"
        } else {
            "数据没拨上：查流量开关、APN 或欠费"
        };
        return say(o, Cause::None, Tone::Bad, "没连上网", h.into());
    }
    if capped {
        return say(
            o,
            Cause::Limit,
            Tone::Warn,
            "慢：限速",
            format!("运营商限到 {} Mbps，换位置没用", to_int(i.ambr_dl + 0.5) as i32),
        );
    }
    if st == 0 || (i.rsrp_valid && i.rsrp < -110) {
        if i.rsrp_valid {
            return say(o, Cause::Weak, Tone::Warn, "慢：信号弱", format!("RSRP {}：离基站远，靠窗通常好些{roam_note}", i.rsrp));
        }
        return say(o, Cause::Weak, Tone::Warn, "慢：信号弱", format!("离基站远，靠窗通常好些{roam_note}"));
    }
    if nq == 0 {
        return say(
            o,
            Cause::Noise,
            Tone::Warn,
            "慢：干扰大",
            format!("SINR {}：杂波多，挪个位置或换个朝向{roam_note}", fmt1(i.sinr)),
        );
    }
    if crowd {
        return say(o, Cause::Crowd, Tone::Warn, "慢：疑似拥挤", format!("RSRQ {}：人多抢网，换位置帮助不大", i.rsrq));
    }
    if rat == Rat::G2 {
        let h = if pin.is_some() { "制式被限定只用 2G，去「锁频」改回" } else { "上网会非常慢，附近可能没有 4G/5G" };
        return say(o, Cause::None, Tone::Warn, "只有 2G", h.into());
    }
    if rat == Rat::G3 {
        let h = if pin.is_some() { "制式被限定只用 3G，去「锁频」改回" } else { "能上网但较慢，附近可能没有 4G/5G" };
        return say(o, Cause::None, Tone::Warn, "只有 3G", h.into());
    }
    if narrow {
        return say(o, Cause::Narrow, Tone::Warn, "慢：载波窄", format!("这里只给了 1 条 {} MHz", i.mhz));
    }
    if pin.is_some() && rat == Rat::G4 {
        return say(o, Cause::None, Tone::Ok, "顺畅", "制式限定只用 4G，去「锁频」改回".into());
    }
    say(o, Cause::None, Tone::Ok, "顺畅", String::new())
}

/// C `%.1f` / `%.0f` (round half to even on the exact binary value, like
/// glibc/musl; Rust's formatter rounds the same way).
fn fmt1(x: f64) -> String {
    format!("{x:.1}")
}

fn fmt0(x: f64) -> String {
    format!("{x:.0}")
}

// ---- net_view.c -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Carrier {
    /// "nr" / "lte"
    pub kind: &'static str,
    /// 0 = not in the carrier record (the serving NR cell)
    pub band: i64,
    pub pci: i64,
    pub bw: i64,
    pub active: bool,
    pub arfcn: i64,
    /// Display text: RSRP / RSRQ as `%.0f`, SINR as `%.1f`.
    pub rsrp: String,
    pub rsrq: String,
    pub sinr: String,
    /// Row label: n78 / B3 / net.nr_band as-is / "-".
    pub label: String,
    /// Summary label: n78 / B3.
    pub label_short: String,
    #[serde(skip)]
    rsrp_v: f64,
    #[serde(skip)]
    rsrq_v: f64,
    #[serde(skip)]
    sinr_v: f64,
}

fn parse_ca(s: &str, max: usize, nr: bool) -> Vec<Carrier> {
    let kind = if nr { "nr" } else { "lte" };
    let mut out = Vec::new();
    let buf = cstr(s.to_string(), 256);
    for rec in buf.split(';').filter(|r| !r.is_empty()) {
        if out.len() >= max {
            break;
        }
        let mut f = [0f64; 11];
        let nf = scan_floats(rec, 11, &mut f);
        let c = |pci: f64, band: f64, arfcn: f64, bw: f64, rsrp: f64, rsrq: f64, sinr: f64, active: bool| Carrier {
            kind,
            band: to_int(band) as i32 as i64,
            pci: to_int(pci) as i32 as i64,
            bw: to_int(bw) as i32 as i64,
            active,
            arfcn: to_int(arfcn),
            rsrp: String::new(),
            rsrq: String::new(),
            sinr: String::new(),
            label: String::new(),
            label_short: String::new(),
            rsrp_v: rsrp,
            rsrq_v: rsrq,
            sinr_v: sinr,
        };
        if nf == 5 {
            // the 5-field lteca "PCI,band,?,EARFCN,bw" (B27 firmware): no signal values
            out.push(c(f[0], f[1], f[3], f[4], 0.0, 0.0, 0.0, true));
            continue;
        }
        if nf != 11 {
            continue;
        }
        // RSRP at the -140 floor = configured but not scheduled
        out.push(c(f[1], f[3], f[4], f[5], f[7], f[8], f[9], f[7] > -140.0));
    }
    out
}

/// Everything the home signal card and the status bar derive.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct NetView {
    pub carriers: Vec<Carrier>,
    pub act_n: i64,
    pub act_bw: i64,
    pub act_nr: i64,
    pub act_lte: i64,
    /// Configured carriers, de-duplicated by kind + PCI + band.
    pub cfg: i64,
    pub nr_band0: i64,
    pub nr_mhz: i64,
    pub roam_known: bool,
    pub roam: bool,
    /// The verdict for this snapshot (the screen holds a change for 15 s).
    pub story: Story,
    /// Headline is 无服务 / 只能紧急呼叫.
    pub nosvc: bool,
    pub sim_usable: bool,
    pub have_home: bool,
    pub home_mcc: i64,
    pub home_mnc: i64,
    /// Roaming on a different network than the SIM's own.
    pub other: bool,
    /// SIM operator logo slug, "" = none.
    pub logo: String,
    /// 5G SA / 5G NSA · 4G 锚点 / 4G LTE-A …
    pub fine: String,
    /// Operator name, or 未注册.
    pub name: String,
    /// 漫游到X / 漫游 / 本地 / "".
    pub r#where: String,
    /// 激活 a/c · N 载波聚合 · 单载波 · 无聚合 · 没连上基站 · —
    pub ca_val: String,
    /// ↓ list   ↑ primary, the 3G/2G band, or "".
    pub ca_sub: String,
    /// Status-bar dot colour: 2 green, 1 orange, 0 red, -1 none.
    pub bars_tier: i64,
}

fn carriers(d: &Data, v: &mut NetView) {
    let mut ca: Vec<Carrier> = Vec::new();
    // an idle NSA leg still reports nr5g_rsrp but has no band, bandwidth or channel
    if d.nr_rsrp != 0 && (!d.nr_band.is_empty() || atoi(&d.nr_bw) > 0 || d.nr_channel > 0) {
        ca.push(Carrier {
            kind: "nr",
            band: 0,
            pci: d.nr_pci,
            bw: atoi(&d.nr_bw) as i32 as i64,
            active: true,
            arfcn: d.nr_channel,
            rsrp: String::new(),
            rsrq: String::new(),
            sinr: String::new(),
            label: String::new(),
            label_short: String::new(),
            rsrp_v: d.nr_rsrp as f64,
            rsrq_v: d.nr_rsrq as f64,
            sinr_v: atof(if d.nr_snr.is_empty() { "0" } else { &d.nr_snr }),
        });
    }
    let room = CA_MAX - ca.len();
    ca.extend(parse_ca(&d.nrca, room, true));
    let rat = rat_of(&d.net_type);
    if ca.len() < CA_MAX {
        let mut lte = parse_ca(&d.lteca, CA_MAX - ca.len(), false);
        let lte_snr = atof(if d.lte_snr.is_empty() { "0" } else { &d.lte_snr });
        for c in &mut lte {
            if c.rsrp_v == 0.0 && c.pci == d.lte_pci && d.lte_rsrp != 0 {
                c.rsrp_v = d.lte_rsrp as f64;
                c.rsrq_v = d.lte_rsrq as f64;
                c.sinr_v = lte_snr;
            }
        }
        let lte_n = lte.len();
        ca.extend(lte);
        if lte_n == 0 && d.lte_rsrp != 0 && matches!(rat, Rat::G4 | Rat::Nsa) {
            let bs = band_short(&d.band, false);
            ca.push(Carrier {
                kind: "lte",
                band: atoi_after_first_byte(&bs),
                pci: d.lte_pci,
                bw: atoi(&d.bandwidth) as i32 as i64,
                active: true,
                arfcn: d.channel,
                rsrp: String::new(),
                rsrq: String::new(),
                sinr: String::new(),
                label: String::new(),
                label_short: String::new(),
                rsrp_v: d.lte_rsrp as f64,
                rsrq_v: d.lte_rsrq as f64,
                sinr_v: lte_snr,
            });
        }
    }
    for c in &mut ca {
        let nr = c.kind == "nr";
        let p = if nr { 'n' } else { 'B' };
        if c.band != 0 {
            c.label = cstr(format!("{p}{}", c.band), 16);
            c.label_short = c.label.clone();
        } else if nr {
            c.label = cstr(if d.nr_band.is_empty() { "-".into() } else { d.nr_band.clone() }, 16);
            c.label_short = band_short(&d.nr_band, true);
        } else {
            c.label = band_short(&d.band, false);
            c.label_short = c.label.clone();
        }
        c.rsrp = fmt0(c.rsrp_v);
        c.rsrq = fmt0(c.rsrq_v);
        c.sinr = fmt1(c.sinr_v);
        if c.active {
            v.act_n += 1;
            v.act_bw += c.bw;
            if nr {
                v.act_nr += 1;
            } else {
                v.act_lte += 1;
            }
        }
    }
    for c in ca.iter().filter(|c| c.active && c.kind == "nr") {
        if v.nr_band0 == 0 {
            v.nr_band0 = if c.band != 0 {
                c.band
            } else {
                atoi_after_first_byte(&band_short(&d.nr_band, true))
            };
        }
        v.nr_mhz += c.bw;
    }
    v.carriers = ca;
}

fn summary(d: &Data, v: &mut NetView) {
    let ca = &v.carriers;
    for i in 0..ca.len() {
        let dup = (0..i).any(|j| {
            ca[j].kind == ca[i].kind
                && ca[j].pci == ca[i].pci
                && (ca[j].band == ca[i].band || ca[j].band == 0 || ca[i].band == 0)
        });
        if !dup {
            v.cfg += 1;
        }
    }
    // char list[120] filled with snprintf while lo + 16 < 120
    let mut list = String::new();
    let mut lo: usize = 0;
    let mut first = String::new();
    for c in ca {
        if lo + 16 >= 120 {
            break;
        }
        if !c.active {
            continue;
        }
        if first.is_empty() {
            first = cstr(c.label_short.clone(), 20);
        }
        let piece = if c.bw != 0 {
            format!("{}{} {}M", if lo > 0 { " + " } else { "" }, c.label_short, c.bw)
        } else {
            format!("{}{}", if lo > 0 { " + " } else { "" }, c.label_short)
        };
        lo += piece.len();
        list.push_str(&piece);
    }
    let list = cstr(list, 120);
    let r2 = rat_of(&d.net_type);
    if v.act_n == 0 && matches!(r2, Rat::G3 | Rat::G2) {
        v.ca_val = "无聚合".into();
        v.ca_sub = if d.band.is_empty() { String::new() } else { band_short(&d.band, false) };
    } else if v.act_n == 0 {
        v.ca_val = if v.nosvc { "没连上基站" } else { "—" }.into();
        v.ca_sub = String::new();
    } else {
        v.ca_val = if v.cfg > v.act_n {
            format!("激活 {}/{}", v.act_n, v.cfg)
        } else if v.act_n > 1 {
            format!("{} 载波聚合", v.act_n)
        } else {
            "单载波".into()
        };
        v.ca_sub = cstr(format!("↓ {list}   ↑ {first}"), 160);
    }
}

pub fn net_view(state: &Value) -> NetView {
    let d = parse(state);
    let mut v = NetView::default();
    carriers(&d, &mut v);

    v.roam_known = !d.roaming.is_empty();
    v.roam = !d.roaming.is_empty() && d.roaming != "Home" && d.roaming != "home" && d.roaming != "0";

    let ws = &d.wan_status;
    let c0 = v.carriers.first();
    let nin = NetIn {
        sim_state: &d.sim_state,
        airplane: d.operate_mode.contains("LPM") || d.operate_mode.contains("OFFLINE"),
        net_type: &d.net_type,
        bars: d.bars,
        data_up: ws.is_empty() || (ws.contains("connected") && !ws.contains("disconnect")),
        roaming: if d.roaming.is_empty() { -1 } else { i64::from(v.roam) },
        n_active: v.act_n,
        nr_active: v.act_nr,
        lte_active: v.act_lte,
        mhz: v.act_bw,
        sinr_valid: c0.is_some(),
        sinr: c0.map_or(0.0, |c| c.sinr_v),
        rsrp_valid: c0.is_some(),
        rsrp: c0.map_or(0, |c| to_int(c.rsrp_v) as i32 as i64),
        rsrq_valid: c0.is_some_and(|c| c.rsrq_v != 0.0),
        rsrq: c0.map_or(0, |c| to_int(c.rsrq_v) as i32 as i64),
        mcc: d.mcc,
        mnc: d.mnc,
        nr_band: v.nr_band0,
        nr_mhz: v.nr_mhz,
        rx_bps: d.rx_speed,
        ambr_dl: d.ambr_dl,
        net_select: &d.net_select,
        data_sw: match d.cell_data {
            0 => Sw::Off,
            1 => Sw::On,
            _ => Sw::Unknown,
        },
        roam_sw: match d.cell_roam {
            0 => Sw::Off,
            1 => Sw::On,
            _ => Sw::Unknown,
        },
    };
    v.story = story(&nin);
    v.nosvc = v.story.headline == "无服务" || v.story.headline == "只能紧急呼叫";

    let home = imsi_plmn(&d.sim_imsi);
    v.have_home = home.is_some();
    (v.home_mcc, v.home_mnc) = home.unwrap_or((d.mcc, d.mnc));
    v.other = v.have_home && v.roam && d.mcc > 0 && (v.home_mcc != d.mcc || v.home_mnc != d.mnc);
    v.sim_usable = sim_usable(&d.sim_state);
    v.logo = if v.sim_usable { sim_logo(v.home_mcc, v.home_mnc, &d.sim_spn).unwrap_or("") } else { "" }.into();

    v.fine = rat_long(&d.net_type, v.act_lte);
    v.name = if d.operator_name.is_empty() { "未注册".into() } else { d.operator_name.clone() };
    v.r#where = if v.nosvc || d.roaming.is_empty() {
        String::new()
    } else if v.other {
        cstr(format!("漫游到{}", v.name), 64)
    } else if v.roam {
        "漫游".into()
    } else {
        "本地".into()
    };

    summary(&d, &mut v);
    v.bars_tier = bars_tier(d.bars);
    v
}

#[cfg(test)]
mod tests;
