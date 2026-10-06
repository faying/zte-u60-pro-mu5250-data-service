use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

const DEFAULT_CURVE: [(i64, i64); 5] = [(40, 0), (45, 0), (50, 76), (60, 128), (70, 255)];
static HARD_OVERRIDE: AtomicBool = AtomicBool::new(false);
static COOLING_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[derive(Clone)]
struct Config {
    fan_always: bool,
    fan_mode: i64,
    fan_speed: i64,
    liquid_always: bool,
    liquid_level: i64,
    temps: [i64; 3],
    hyst: [i64; 3],
    curve: Vec<(i64, i64)>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            fan_always: false,
            fan_mode: 2,
            fan_speed: 50,
            liquid_always: false,
            liquid_level: 1,
            temps: [44, 48, 53],
            hyst: [4, 4, 4],
            curve: DEFAULT_CURVE.to_vec(),
        }
    }
}
fn env(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .map(PathBuf::from)
        .unwrap_or_else(|_| default.into())
}
#[allow(clippy::field_reassign_with_default)]
async fn load() -> Config {
    let path = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return Config::default();
    };
    let mut map = BTreeMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=')
            && let Ok(v) = v.parse()
        {
            map.insert(k.to_owned(), v);
        }
    }
    let mut c = Config::default();
    c.fan_always = map.get("fan_always_on").copied().unwrap_or(0) != 0;
    c.fan_mode = map.get("fan_mode").copied().unwrap_or_else(|| {
        if map.get("fan_auto").copied().unwrap_or(0) != 0 {
            1
        } else {
            2
        }
    });
    c.fan_speed = map.get("fan_speed_percent").copied().unwrap_or(50);
    c.liquid_always = map
        .get("liquid_always_on")
        .or_else(|| map.get("liquid_enabled"))
        .copied()
        .unwrap_or(0)
        != 0;
    c.liquid_level = map.get("liquid_level").copied().unwrap_or(1).clamp(1, 2);
    for i in 0..3 {
        c.temps[i] = map
            .get(&format!("temperature_{}", i + 1))
            .copied()
            .unwrap_or(c.temps[i]);
        c.hyst[i] = map
            .get(&format!("hysteresis_{}", i + 1))
            .copied()
            .unwrap_or(c.hyst[i]);
    }
    let count = map
        .get("custom_curve_count")
        .copied()
        .unwrap_or(5)
        .clamp(2, 8);
    c.curve = (0..count)
        .map(|i| {
            (
                map.get(&format!("custom_temperature_{}", i + 1))
                    .copied()
                    .unwrap_or(DEFAULT_CURVE.get(i as usize).map(|v| v.0).unwrap_or(70)),
                map.get(&format!("custom_pwm_{}", i + 1))
                    .copied()
                    .unwrap_or(DEFAULT_CURVE.get(i as usize).map(|v| v.1).unwrap_or(255)),
            )
        })
        .collect();
    c
}
async fn write(path: PathBuf, value: &str) -> Result<(), String> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?
        .write_all(value.as_bytes())
        .await
        .map_err(|e| e.to_string())
}
use tokio::io::AsyncWriteExt;
fn zone() -> PathBuf {
    env(
        "ZWRT_DATAD_COOLING_ZONE_PATH",
        "/sys/class/thermal/thermal_zone0",
    )
}
async fn temperature() -> i64 {
    tokio::fs::read_to_string(zone().join("temp"))
        .await
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|v| if v >= 1000 { (v + 500) / 1000 } else { v })
        .unwrap_or(-1)
}
async fn prepare_fan() -> Result<(), String> {
    write(
        env(
            "ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH",
            "/sys/class/hwmon/hwmon0/device/thermal_enable",
        ),
        "1",
    )
    .await?;
    write(zone().join("mode"), "disabled").await?;
    write(
        env(
            "ZWRT_DATAD_FAN_COOLING_STATE_PATH",
            "/sys/class/thermal/cooling_device0/cur_state",
        ),
        "0",
    )
    .await
}
async fn fan_pwm(pwm: i64) -> Result<(), String> {
    prepare_fan().await?;
    if let Err(e) = write(
        env("ZWRT_DATAD_FAN_PWM_PATH", "/sys/class/hwmon/hwmon0/pwm1"),
        &pwm.clamp(0, 255).to_string(),
    )
    .await
    {
        let _ = write(zone().join("mode"), "enabled").await;
        return Err(e);
    }
    Ok(())
}
fn curve_pwm(c: &Config, temp: i64) -> Option<i64> {
    if temp >= 80 {
        return Some(255);
    }
    if temp <= 0 {
        return None;
    }
    if temp <= c.curve[0].0 {
        return Some(c.curve[0].1);
    }
    for w in c.curve.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if temp <= x1 {
            return Some(y0 + ((temp - x0) * (y1 - y0) + (x1 - x0) / 2) / (x1 - x0));
        }
    }
    c.curve.last().map(|v| v.1)
}
async fn apply_fan(c: &Config) -> Result<(), String> {
    let temp = temperature().await;
    if temp >= 80 || (temp <= 0 && HARD_OVERRIDE.load(Ordering::Relaxed)) {
        let result = fan_pwm(255).await;
        if result.is_ok() {
            HARD_OVERRIDE.store(true, Ordering::Relaxed);
        }
        return result;
    }
    if c.fan_always {
        return fan_pwm(128).await;
    }
    if c.fan_mode == 1 {
        write(
            env(
                "ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH",
                "/sys/class/hwmon/hwmon0/device/thermal_enable",
            ),
            "1",
        )
        .await?;
        for i in 0..3 {
            write(
                zone().join(format!("trip_point_{i}_temp")),
                &(c.temps[i] * 1000).to_string(),
            )
            .await?;
            write(
                zone().join(format!("trip_point_{i}_hyst")),
                &(c.hyst[i] * 1000).to_string(),
            )
            .await?;
        }
        let result = write(zone().join("mode"), "enabled").await;
        if result.is_ok() {
            HARD_OVERRIDE.store(false, Ordering::Relaxed);
        }
        return result;
    }
    let pwm = curve_pwm(c, temp).ok_or("fan temperature unavailable")?;
    let result = fan_pwm(pwm).await;
    if result.is_ok() {
        HARD_OVERRIDE.store(false, Ordering::Relaxed);
    }
    result
}
async fn apply_liquid(c: &Config) -> Result<(), String> {
    let thermal = env(
        "ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH",
        "/sys/class/leds/aw_vibrator/thermal_enable",
    );
    let drive = env(
        "ZWRT_DATAD_LIQUID_DRIVE_PATH",
        "/sys/class/leds/aw_vibrator/atsin0",
    );
    if c.liquid_always {
        write(thermal, "0").await?;
        write(
            drive,
            &format!("1023 {} 200", if c.liquid_level >= 2 { 200 } else { 60 }),
        )
        .await?;
        Ok(())
    } else {
        write(drive, "0 0 0").await?;
        write(thermal, "1").await
    }
}

pub async fn tick() {
    let _guard = COOLING_LOCK.lock().await;
    let path = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    if tokio::fs::metadata(path).await.is_err() {
        return;
    }
    let config = load().await;
    if config.liquid_always {
        let _ = apply_liquid(&config).await;
    }
    let _ = apply_fan(&config).await;
}
