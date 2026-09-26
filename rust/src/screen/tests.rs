//! Parity with the screen's C (touch-ui tests/parity/gen.py): every case in
//! tests/fixtures/screen_net_corpus.jsonl must come out field for field the same.

use super::*;
use std::path::PathBuf;

fn merge(base: &mut Value, patch: &Value) {
    let (Some(b), Some(p)) = (base.as_object_mut(), patch.as_object()) else { return };
    for (k, v) in p {
        if v.is_null() {
            b.remove(k);
        } else if v.is_object() && b.get(k).is_some_and(Value::is_object) {
            merge(b.get_mut(k).unwrap(), v);
        } else {
            b.insert(k.clone(), v.clone());
        }
    }
}

#[test]
fn matches_the_screens_c_on_the_corpus() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let template: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("../tests/golden/normal.state.json")).unwrap()).unwrap();
    let corpus = std::fs::read_to_string(root.join("tests/fixtures/screen_net_corpus.jsonl")).unwrap();
    let mut n = 0;
    let mut bad = Vec::new();
    for line in corpus.lines().skip(1) {
        let case: Value = serde_json::from_str(line).unwrap();
        let mut state = template.clone();
        merge(&mut state, &case["patch"]);
        let got = serde_json::to_value(net_view(&state)).unwrap();
        let mut want = case["view"].clone();
        want.as_object_mut().unwrap().remove("parsed");
        n += 1;
        if got != want {
            let (g, w) = (got.as_object().unwrap(), want.as_object().unwrap());
            let mut diff: Vec<String> = w
                .iter()
                .filter(|(k, v)| g.get(*k) != Some(v))
                .map(|(k, v)| format!("{k}: want {v} got {}", g.get(k).map_or("∅".into(), |x| x.to_string())))
                .collect();
            diff.extend(g.keys().filter(|k| !w.contains_key(*k)).map(|k| format!("{k}: extra")));
            bad.push(format!("case {}: {}", case["id"], diff.join("; ")));
        }
    }
    assert!(n > 1000, "corpus too small: {n}");
    assert!(bad.is_empty(), "{} of {n} cases differ:\n{}", bad.len(), bad.iter().take(15).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn c_scalar_reads() {
    assert_eq!(strtol("  -12x"), Some(-12));
    assert_eq!(strtol("x"), None);
    assert_eq!(atoi("18.0"), 18);
    assert_eq!(atof("-3.25dB"), -3.25);
    assert_eq!(atof(""), 0.0);
    let mut f = [0f64; 11];
    assert_eq!(scan_floats("263,3,0,1750,20", 11, &mut f), 5);
    assert_eq!(scan_floats("1,2,x", 11, &mut f), 2);
    assert_eq!(scan_floats("0,17,0,78,627264,100,0,-90,-10,15.5,-60,9", 11, &mut f), 11);
    assert_eq!(band_short("LTE BAND 3", false), "B3");
    assert_eq!(band_short("GSM 900", false), "GSM 900");
    assert_eq!(band_short("", true), "-");
    assert_eq!(atoi_after_first_byte("-"), 0);
    assert_eq!(atoi_after_first_byte("n78"), 78);
    assert_eq!(cstr("abcdef".into(), 4), "abc");
    assert_eq!(cstr("中国移动".into(), 5), "中");
}

// ---- the named cases the screen's C tests had (touch-ui tests/ui_logic_test.c,
// parity-net-v1), moved with the rules ----

fn base() -> NetIn<'static> {
    NetIn {
        sim_state: "sim ready",
        airplane: false,
        net_type: "SA",
        bars: 5,
        data_up: true,
        roaming: 0,
        n_active: 3,
        nr_active: 3,
        lte_active: 0,
        mhz: 220,
        sinr_valid: true,
        sinr: 17.7,
        rsrp_valid: true,
        rsrp: -87,
        rsrq_valid: true,
        rsrq: -11,
        mcc: 460,
        mnc: 11,
        nr_band: 78,
        nr_mhz: 220,
        rx_bps: 0,
        ambr_dl: 1668.64,
        net_select: "WL_AND_5G",
        data_sw: Sw::Unknown,
        roam_sw: Sw::Unknown,
    }
}

fn with(f: impl FnOnce(&mut NetIn<'static>)) -> Story {
    let mut x = base();
    f(&mut x);
    story(&x)
}

#[test]
fn radio_names_and_bands() {
    assert_eq!(rat_of("ENDC"), Rat::Nsa);
    assert_eq!(rat_of("LTE-A"), Rat::G4);
    assert_eq!(rat_of("WCDMA"), Rat::G3);
    assert_eq!(rat_of("GSM"), Rat::G2);
    assert_eq!(rat_of("UNREGISTERED"), Rat::None);
    assert_eq!(rat_of("LIMITED_SERVICE_SA"), Rat::None);
    assert_eq!(rat_of("NR5G_SA"), Rat::Sa);
    for (raw, lte, want) in [
        ("NSA", 1, "5G NSA · 4G 锚点"),
        ("SA", 1, "5G SA"),
        ("HSPA+", 1, "3G HSPA+"),
        ("EDGE", 1, "2G EDGE"),
        ("CDMA2000", 1, "3G CDMA2000"),
        ("LIMITED_SERVICE", 1, ""),
        ("LTE", 1, "4G LTE"),
        ("LTE", 2, "4G LTE-A"),
        ("WCDMA", 1, "3G WCDMA"),
        ("GSM", 1, "2G GSM"),
    ] {
        assert_eq!(rat_long(raw, lte), want, "{raw}");
    }
    for (raw, nr, want) in [
        ("LTE BAND 3", false, "B3"),
        ("NR5G BAND 78", true, "n78"),
        ("n78", true, "n78"),
        ("B41", false, "B41"),
        ("", false, "-"),
        ("DCS", false, "DCS"),
        ("GSM 900", false, "GSM 900"),
        ("n261", true, "n261"),
    ] {
        assert_eq!(band_short(raw, nr), want, "{raw}");
    }
}

#[test]
fn story_every_situation_in_priority_order() {
    let o = with(|_| {});
    assert_eq!((o.headline.as_str(), o.tone), ("顺畅", Tone::Ok));
    assert_eq!((o.rat.as_str(), o.link.as_str()), ("5G-A", "3 条载波聚合 · 带宽很宽"));
    assert_eq!((o.sig.as_str(), o.sig_tone, o.noise.as_str(), o.load.as_str(), o.limit.as_str(), o.cause),
               ("强", Tone::Ok, "小", "", "无", Cause::None));
    assert_eq!(o.hint, "");

    let h = |f: fn(&mut NetIn<'static>)| {
        let o = with(f);
        (o.headline, o.tone)
    };
    assert_eq!(h(|x| { x.sim_state = "sim absent"; x.bars = 0 }), ("无 SIM".into(), Tone::Bad));
    assert_eq!(h(|x| { x.airplane = true; x.bars = 0 }), ("移动网络已关".into(), Tone::Neutral));
    assert_eq!(h(|x| { x.net_type = "LIMITED_SERVICE"; x.bars = 0 }), ("只能紧急呼叫".into(), Tone::Bad));
    assert_eq!(h(|x| { x.net_type = ""; x.bars = 0 }), ("无服务".into(), Tone::Bad));
    assert_eq!(h(|x| x.data_up = false), ("没连上网".into(), Tone::Bad));

    assert!(with(|x| { x.data_up = false; x.roaming = 1 }).hint.contains("数据漫游"));
    let o = with(|x| { x.data_up = false; x.roaming = 1; x.roam_sw = Sw::On });
    assert!(o.hint.contains("正在拨号") && !o.hint.contains("打开"));
    assert!(with(|x| { x.data_up = false; x.roaming = 1; x.roam_sw = Sw::Off }).hint.contains("数据漫游关着"));
    assert!(with(|x| { x.data_up = false; x.roaming = 1; x.roam_sw = Sw::Off; x.data_sw = Sw::Off })
        .hint
        .contains("移动数据关着"));

    assert_eq!(h(|x| x.bars = 2), ("慢：信号弱".into(), Tone::Warn));
    assert_eq!(h(|x| x.sinr = -2.5), ("慢：干扰大".into(), Tone::Warn));
    assert_eq!(h(|x| x.rsrp = -115), ("慢：信号弱".into(), Tone::Warn));
    assert_eq!(h(|x| { x.bars = 1; x.roaming = 1 }), ("慢：信号弱".into(), Tone::Warn));
    assert_eq!(h(|x| { x.bars = 2; x.sinr = -3.0 }), ("慢：信号弱".into(), Tone::Warn));
    let o = with(|x| x.sinr = -2.5);
    assert!(o.sig == "强" && o.noise == "大" && o.cause == Cause::Noise && o.hint.contains("SINR -2.5"));
    assert_eq!([5, 4, 3, 2, 1, 0].map(bars_tier), [2, 2, 1, 0, 0, -1]);

    assert_eq!(h(|x| x.roaming = 1), ("顺畅".into(), Tone::Ok));
    assert_eq!(h(|x| { x.net_type = "WCDMA"; x.n_active = 0; x.sinr_valid = false; x.rsrp_valid = false }),
               ("只有 3G".into(), Tone::Warn));
    assert_eq!(h(|x| { x.net_type = "EDGE"; x.n_active = 0; x.sinr_valid = false; x.rsrp_valid = false }),
               ("只有 2G".into(), Tone::Warn));
    let o = with(|x| { x.net_type = "WCDMA"; x.n_active = 0; x.sinr_valid = false; x.rsrp_valid = false; x.net_select = "Only_WCDMA" });
    assert!(o.hint.contains("限定") && o.link == "这个制式没有载波聚合");
    let o = with(|x| { x.net_type = "NSA"; x.n_active = 2; x.nr_active = 1; x.lte_active = 1; x.mhz = 120 });
    assert!(o.headline == "顺畅" && o.rat == "5G" && o.link == "4G 锚点 + 5G，2 条载波 · 带宽充足");
    let o = with(|x| { x.net_type = "LTE"; x.n_active = 3; x.nr_active = 0; x.lte_active = 3; x.mhz = 60 });
    assert!(o.rat == "4G" && o.link == "3 条载波聚合 · 带宽一般");
    let o = with(|x| { x.net_type = "LTE"; x.n_active = 1; x.nr_active = 0; x.lte_active = 1; x.mhz = 20; x.net_select = "Only_LTE" });
    assert!(o.rat == "4G" && o.link == "单载波 · 带宽偏窄" && o.headline == "慢：载波窄" && o.cause == Cause::Narrow);
    let o = with(|x| { x.net_type = "LTE"; x.n_active = 2; x.nr_active = 0; x.lte_active = 2; x.mhz = 40; x.net_select = "Only_LTE" });
    assert!(o.headline == "顺畅" && o.hint.contains("只用 4G") && o.tone == Tone::Ok);
    assert_eq!(with(|x| { x.net_type = "LTE"; x.n_active = 1; x.nr_active = 0; x.lte_active = 1; x.mhz = 0 }).link, "单载波");
    let o = with(|x| x.net_select = "TCHGWL_5G");
    assert!(o.hint.is_empty() || !o.hint.contains("只用"));
    let o = with(|x| x.bars = 3);
    assert!(o.sig == "中" && o.sig_tone == Tone::Warn);
    let o = with(|x| x.sinr = 5.0);
    assert!(o.noise == "中" && o.sig == "强");
    let o = with(|x| x.sinr = -3.0);
    assert!(o.noise == "大" && o.noise_tone == Tone::Bad);

    // why it is slow
    assert_eq!(h(|x| x.ambr_dl = 5.0), ("慢：限速".into(), Tone::Warn));
    let o = with(|x| x.ambr_dl = 5.0);
    assert!(o.cause == Cause::Limit && o.limit == "有" && o.hint.contains("5 Mbps"));
    let o = with(|x| x.ambr_dl = 0.0);
    assert!(o.limit == "—" && o.headline == "顺畅");
    assert_eq!(h(|x| { x.ambr_dl = 5.0; x.sinr = -3.0 }), ("慢：限速".into(), Tone::Warn));
    assert_eq!(h(|x| { x.rsrq = -18; x.rx_bps = 400_000 }), ("慢：疑似拥挤".into(), Tone::Warn));
    let o = with(|x| { x.rsrq = -18; x.rx_bps = 400_000 });
    assert!(o.cause == Cause::Crowd && o.load == "高" && o.sig == "强" && o.hint.contains("RSRQ -18"));
    assert_eq!(h(|x| { x.rsrq = -18; x.rx_bps = 20_000 }), ("顺畅".into(), Tone::Ok));
    assert_eq!(with(|x| x.rsrq = -18).load, "");
    assert_eq!(with(|x| x.rx_bps = 400_000).load, "正常");
    assert_eq!(h(|x| { x.net_type = "LTE"; x.n_active = 2; x.lte_active = 2; x.nr_active = 0; x.mhz = 40; x.rsrq = -13; x.rx_bps = 400_000 }),
               ("慢：疑似拥挤".into(), Tone::Warn));
    assert_eq!(h(|x| { x.rsrp = -104; x.sinr = -1.0; x.rsrq = -18; x.rx_bps = 400_000 }), ("慢：干扰大".into(), Tone::Warn));
    assert_eq!(h(|x| { x.n_active = 1; x.nr_active = 1; x.mhz = 20 }), ("慢：载波窄".into(), Tone::Warn));
    assert_eq!(with(|x| { x.n_active = 1; x.nr_active = 1; x.mhz = 100 }).headline, "顺畅");
    assert_eq!(h(|x| { x.n_active = 1; x.nr_active = 1; x.mhz = 15; x.roaming = 1 }), ("慢：载波窄".into(), Tone::Warn));

    // 2026-09-25 on the device: SA, one n5 15 MHz carrier, SINR -1.9, RSRP -102, RSRQ -17
    let o = with(|x| { x.bars = 3; x.n_active = 1; x.nr_active = 1; x.mhz = 15; x.sinr = -1.9; x.rsrp = -102; x.rsrq = -17; x.rx_bps = 0 });
    assert!(o.headline == "慢：干扰大" && o.sig == "中" && o.noise == "大" && o.load.is_empty() && o.limit == "无"
        && o.cause == Cause::Noise && o.hint.contains("SINR -1.9"));
    assert!(o.headline.len() <= 18 && o.hint.len() <= 60);
}

#[test]
fn status_bar_label_by_the_network_you_are_on() {
    let z = || {
        let mut x = base();
        x.net_type = "SA";
        x.mcc = 460;
        x.mnc = 11;
        x.nr_active = 1;
        x.nr_band = 78;
        x.nr_mhz = 100;
        x.mhz = 100;
        x
    };
    let b = |f: fn(&mut NetIn<'static>)| {
        let mut x = z();
        f(&mut x);
        net_badge(&x)
    };
    assert_eq!(b(|_| {}), "5G");
    assert_eq!(b(|x| { x.nr_active = 2; x.nr_mhz = 200 }), "5G");
    assert_eq!(b(|x| x.nr_active = 3), "5G-A");
    assert_eq!(b(|x| { x.net_type = "NSA"; x.nr_active = 3 }), "5G-A");
    assert_eq!(b(|x| { x.mnc = 1; x.nr_active = 2; x.nr_mhz = 200 }), "5G-A");
    assert_eq!(b(|x| { x.mnc = 1; x.nr_active = 2; x.nr_mhz = 160 }), "5G");
    assert_eq!(b(|x| { x.mcc = 310; x.mnc = 260; x.nr_band = 41 }), "5G UC");
    assert_eq!(b(|x| { x.mcc = 310; x.mnc = 260; x.nr_band = 71 }), "5G");
    assert_eq!(b(|x| { x.mcc = 311; x.mnc = 480; x.nr_band = 77 }), "5G UW");
    assert_eq!(b(|x| { x.mcc = 311; x.mnc = 480; x.nr_band = 48 }), "5G UW");
    assert_eq!(b(|x| { x.mcc = 311; x.mnc = 480; x.nr_band = 5; x.mhz = 10 }), "5G");
    assert_eq!(b(|x| { x.mcc = 310; x.mnc = 410; x.nr_band = 77 }), "5G+");
    assert_eq!(b(|x| { x.mcc = 310; x.mnc = 410; x.nr_band = 5; x.mhz = 50 }), "5G+");
    assert_eq!(b(|x| { x.mcc = 310; x.mnc = 410; x.nr_band = 5; x.mhz = 10 }), "5G");
    assert_eq!(b(|x| { x.mcc = 440; x.mnc = 10; x.nr_active = 3 }), "5G");
    assert_eq!(b(|x| { x.mcc = 466; x.mnc = 92; x.nr_active = 3 }), "5G");
    assert_eq!(b(|x| { x.mcc = 454; x.mnc = 12; x.nr_active = 3 }), "5G");
    assert_eq!(b(|x| { x.mcc = 234; x.mnc = 30; x.nr_active = 3 }), "5G");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 440; x.nr_active = 0; x.lte_active = 2 }), "4G+");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 466; x.nr_active = 0; x.lte_active = 3 }), "4G+");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 466; x.nr_active = 0; x.lte_active = 1 }), "4G");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.nr_active = 0; x.lte_active = 3 }), "4G");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 234; x.nr_active = 0; x.lte_active = 2 }), "4G");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 310; x.mnc = 260; x.nr_active = 0; x.lte_active = 1 }), "LTE");
    assert_eq!(b(|x| { x.net_type = "LTE"; x.mcc = 311; x.mnc = 480; x.nr_active = 0; x.lte_active = 2 }), "LTE");
    assert_eq!(b(|x| { x.net_type = "HSPA+"; x.nr_active = 0 }), "3G");
    assert_eq!(b(|x| { x.net_type = "EDGE"; x.nr_active = 0 }), "2G");
    assert_eq!(b(|x| x.net_type = "LIMITED_SERVICE"), "SOS");
    assert_eq!(b(|x| { x.roaming = 1; x.mcc = 310; x.mnc = 260; x.nr_band = 41 }), "5G UC");
    assert_eq!(b(|x| { x.roaming = 1; x.mcc = 440; x.mnc = 20; x.nr_active = 3 }), "5G");
    let o = with(|x| { x.net_type = ""; x.bars = 0; x.data_up = false });
    assert!(o.rat == "无服务" && o.headline == "无服务");
}

#[test]
fn logos_and_imsi() {
    assert_eq!(operator_logo(460, 0), Some("china-mobile"));
    assert_eq!(operator_logo(460, 11), Some("china-telecom"));
    assert_eq!(operator_logo(311, 480), Some("verizon"));
    assert_eq!(sim_logo(454, 12, "CMLink"), Some("cmlink"));
    assert_eq!(sim_logo(454, 12, ""), Some("cmhk"));
    assert_eq!(operator_logo(455, 1), Some("ctm"));
    assert_eq!(operator_logo(454, 3), Some("three-hk"));
    assert_eq!(imsi_plmn("460011234567890"), Some((460, 1)));
    assert_eq!(imsi_plmn("310260123456789"), Some((310, 260)));
    assert_eq!(imsi_plmn("466920123456789"), Some((466, 92)));
    assert_eq!(imsi_plmn(""), None);
    assert_eq!(operator_logo(0, 0), None);
    assert_eq!(operator_logo(234, 15), None);
}

#[test]
fn phone_style_labels_and_families() {
    for (raw, want, fam) in [
        ("GSM", "2G", "GSM"),
        ("GPRS", "2G", "GPRS"),
        ("EDGE", "2G", "EDGE"),
        ("CDMA", "2G", "CDMA 1X"),
        ("1xRTT", "2G", "CDMA 1X"),
        ("WCDMA", "3G", "WCDMA"),
        ("UMTS", "3G", "WCDMA"),
        ("HSPA", "3G", "HSPA"),
        ("HSPA+", "3G", "HSPA+"),
        ("DC-HSPA+", "3G", "HSPA+"),
        ("TD-SCDMA", "3G", "TD-SCDMA"),
        ("CDMA2000", "3G", "CDMA2000"),
        ("EVDO", "3G", "CDMA2000"),
        ("eHRPD", "3G", "CDMA2000"),
        ("LTE", "4G", "LTE"),
        ("TD-LTE", "4G", "LTE"),
        ("FDD-LTE", "4G", "LTE"),
        ("4G", "4G", "LTE"),
        ("LTE", "4G", "LTE"),
        ("LTE-A", "4G", "LTE"),
        ("LTE_CA", "4G", "LTE"),
        ("4G+", "4G", "LTE"),
        ("SA", "5G", "NR"),
        ("NSA", "5G", "NR"),
        ("ENDC", "5G", "NR"),
        ("SA", "5G", "NR"),
        ("NSA", "5G", "NR"),
        ("SA", "5G", "NR"),
        ("5G-A", "5G-A", "NR"),
        ("LIMITED_SERVICE", "", ""),
        ("LIMITED_SERVICE_SA", "", ""),
        ("", "", ""),
    ] {
        assert_eq!((net_label(raw), rat_family(raw)), (want, fam), "{raw}");
    }
}
