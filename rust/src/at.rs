//! The few AT commands datad sends (write-op-layer.md T7b, D17): going back to
//! automatic network selection and bringing the radio online. A fixed list,
//! never a string from a client.
//!
//! The AT port is shared with zte-agent (calls, USSD, its AT terminal, reads):
//! two readers on one tty take each other's replies, so both sides hold the
//! same cross-process lock (`ZWRT_DATAD_AT_LOCK`, default `/var/run/u60-at.lock`,
//! flock) for the whole command. Port: `ZWRT_DATAD_AT_PORT`, else the first of
//! the agent's list that answers "AT" with OK.

use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

/// Same order as zte-agent `at_cmd.rs`.
const PORTS: &[&str] = &[
    "/dev/at_mdm0",
    "/dev/at_mdm1",
    "/dev/at_usb0",
    "/dev/smd7",
    "/dev/smd11",
];
/// How long to wait for the agent to finish its command on the port.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// An answer ends with OK or ERROR; the agent waits a fixed 8 s for COPS=0,
/// we stop at the answer and give up a little earlier.
const ANSWER_WAIT: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// `AT+COPS=0`: automatic network selection.
    CopsAuto,
    /// `AT+CFUN=1`: radio online (the vendor's nwinfo_set_mode ONLINE does
    /// not bring the modem back from LPM).
    CfunOnline,
}

impl Cmd {
    fn text(self) -> &'static str {
        match self {
            Cmd::CopsAuto => "AT+COPS=0",
            Cmd::CfunOnline => "AT+CFUN=1",
        }
    }
}

fn lock_path() -> Option<String> {
    match std::env::var("ZWRT_DATAD_AT_LOCK") {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v),
        Err(_) => Some("/var/run/u60-at.lock".into()),
    }
}

/// Send `cmd`, return the modem's answer (must contain OK).
pub async fn send(cmd: Cmd) -> Result<String, String> {
    tokio::task::spawn_blocking(move || send_blocking(cmd))
        .await
        .map_err(|e| format!("AT task: {e}"))?
}

fn take_lock() -> Result<Option<File>, String> {
    let Some(path) = lock_path() else {
        return Ok(None);
    };
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("AT lock {path}: {e}"))?;
    let start = Instant::now();
    loop {
        if f.try_lock_exclusive().is_ok() {
            return Ok(Some(f));
        }
        if start.elapsed() >= LOCK_WAIT {
            return Err("AT port busy (another command is running)".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn send_blocking(cmd: Cmd) -> Result<String, String> {
    let _lock = take_lock()?;
    let port = port()?;
    exchange(&port, cmd.text(), ANSWER_WAIT).and_then(|a| {
        if a.contains("OK") {
            Ok(a)
        } else {
            Err(format!("{}: {}", cmd.text(), a.trim()))
        }
    })
}

fn port() -> Result<String, String> {
    if let Ok(p) = std::env::var("ZWRT_DATAD_AT_PORT")
        && !p.is_empty()
    {
        return Ok(p);
    }
    for p in PORTS {
        if Path::new(p).exists()
            && exchange(p, "AT", Duration::from_secs(1)).is_ok_and(|a| a.contains("OK"))
        {
            return Ok((*p).to_string());
        }
    }
    Err("no AT port found".into())
}

/// Write `text\r`, read until OK / ERROR or `wait`.
fn exchange(port: &str, text: &str, wait: Duration) -> Result<String, String> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(port)
        .map_err(|e| format!("open {port}: {e}"))?;
    // drop whatever an earlier command left unread
    let mut junk = [0u8; 512];
    while matches!(f.read(&mut junk), Ok(n) if n > 0) {}
    // non-blocking: a busy port says EAGAIN for a moment
    let line = format!("{text}\r");
    let mut sent = 0;
    let start = Instant::now();
    while sent < line.len() {
        match f.write(&line.as_bytes()[sent..]) {
            Ok(n) => sent += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(1) =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(format!("write {port}: {e}")),
        }
    }
    let start = Instant::now();
    let mut answer = Vec::new();
    let mut buf = [0u8; 512];
    while start.elapsed() < wait {
        match f.read(&mut buf) {
            Ok(n) if n > 0 => {
                answer.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&answer);
                if s.contains("\nOK") || s.starts_with("OK") || s.contains("ERROR") {
                    return Ok(s.into_owned());
                }
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(format!("read {port}: {e}")),
        }
    }
    let s = String::from_utf8_lossy(&answer).trim().to_string();
    Err(if s.is_empty() {
        format!("{text}: no answer")
    } else {
        format!("{text}: {s}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// A pseudo-terminal standing in for the modem: answers `answer` to every
    /// command line it reads; returns the slave path and what it was sent.
    fn fake_modem(answer: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        // SAFETY: plain libc pty calls on a fresh descriptor.
        unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(m >= 0);
            assert_eq!(libc::grantpt(m), 0);
            assert_eq!(libc::unlockpt(m), 0);
            let mut buf = [0 as libc::c_char; 128];
            // ptsname_r: tests run in parallel and ptsname's buffer is shared
            assert_eq!(libc::ptsname_r(m, buf.as_mut_ptr(), buf.len()), 0);
            let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            // raw slave: no echo, no line editing, like the modem's port
            let s = libc::open(
                std::ffi::CString::new(name.clone()).unwrap().as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY,
            );
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(s, &mut t);
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(s, libc::TCSANOW, &t);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _keep_slave_open = s;
                let mut line = Vec::new();
                let mut b = [0u8; 1];
                while libc::read(m, b.as_mut_ptr().cast(), 1) == 1 {
                    if b[0] == b'\r' {
                        let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
                        line.clear();
                        libc::write(m, answer.as_ptr().cast(), answer.len());
                    } else {
                        line.push(b[0]);
                    }
                }
            });
            (name, rx)
        }
    }

    #[test]
    fn sends_only_the_whitelisted_text_and_reads_the_answer() {
        let (port, sent) = fake_modem("\r\nOK\r\n");
        let a = exchange(&port, Cmd::CopsAuto.text(), Duration::from_secs(2)).unwrap();
        assert!(a.contains("OK"));
        assert_eq!(
            sent.recv_timeout(Duration::from_secs(1)).unwrap(),
            "AT+COPS=0"
        );
        let t = Instant::now();
        exchange(&port, Cmd::CfunOnline.text(), Duration::from_secs(2)).unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "stops at the answer");
        assert_eq!(
            sent.recv_timeout(Duration::from_secs(1)).unwrap(),
            "AT+CFUN=1"
        );
    }

    #[test]
    fn error_and_silence_fail() {
        let (port, _) = fake_modem("\r\n+CME ERROR: 3\r\n");
        let a = exchange(&port, "AT+COPS=0", Duration::from_secs(1)).unwrap();
        assert!(a.contains("ERROR"), "{a:?}");
        let (port, _) = fake_modem("");
        let e = exchange(&port, "AT+COPS=0", Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("no answer"), "{e}");
    }

    #[test]
    fn the_lock_is_shared_with_the_agent() {
        let dir = std::env::temp_dir().join(format!("datad-at-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("at.lock");
        let held = File::create(&path).unwrap();
        held.lock_exclusive().unwrap();
        // SAFETY: tests in this module do not read the variable concurrently with the set
        unsafe { std::env::set_var("ZWRT_DATAD_AT_LOCK", &path) };
        let t = Instant::now();
        let r = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            FileExt::unlock(&held).unwrap();
        });
        let got = take_lock().unwrap();
        assert!(got.is_some() && t.elapsed() >= Duration::from_millis(250));
        r.join().unwrap();
    }
}
