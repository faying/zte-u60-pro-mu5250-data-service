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
