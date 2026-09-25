use anyhow::{Context, Result, bail};
use std::{ffi::OsStr, process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

const MAX_OUTPUT: usize = 1024 * 1024;

/// 让子进程随 datad 一起死（SIGKILL 也一样）：Linux 上 fork 后、exec 前调
/// `prctl(PR_SET_PDEATHSIG, SIGKILL)`。
///
/// 细节：pdeathsig 按「fork 出它的那个**线程**」退出触发，不是按进程。datad 的 spawn 发生在
/// tokio 工作线程上，工作线程和运行时同生共死，不会先于进程退出；但不要从 `spawn_blocking`
/// 里起长期子进程（阻塞池线程空闲 10 秒就退出，会误杀子进程）。
/// 另外 fork 之后、prctl 之前父进程可能已经死了（子进程已被过继给 init，prctl 不会再触发），
/// 所以设完再比一次 `getppid()` 和 fork 前记下的父进程号，不一样就立即 `_exit`。
pub fn die_with_parent(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: pre_exec 闭包只调 async-signal-safe 的 prctl / getppid / _exit，不分配内存。
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(0);
                }
                Ok(())
            });
        }
    }
    cmd
}

pub async fn run<I, S>(program: &str, args: I, deadline: Duration) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let child = die_with_parent(&mut Command::new(program))
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    let output = timeout(deadline, child.wait_with_output())
        .await
        .with_context(|| format!("{program} timed out"))??;
    if !output.status.success() {
        bail!("{program} exited with {}", output.status);
    }
    if output.stdout.len() > MAX_OUTPUT {
        bail!("{program} output exceeds limit");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn command_has_timeout_and_no_shell() {
        let out = run("printf", ["%s", "$(id)"], Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out, b"$(id)");
        assert!(
            run("sleep", ["2"], Duration::from_millis(10))
                .await
                .is_err()
        );
    }
}
