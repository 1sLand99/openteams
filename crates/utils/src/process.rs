use std::{io, process::ExitStatus};

use command_group::AsyncGroupChild;
#[cfg(unix)]
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getpgid},
};
use tokio::time::{Duration, Instant};

const FORCE_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ProcessCleanupResult {
    pub exit_status: ExitStatus,
    pub forced_kill: bool,
}

pub async fn terminate_process_group(
    child: &mut AsyncGroupChild,
    graceful_timeout: Duration,
) -> io::Result<ProcessCleanupResult> {
    match tokio::time::timeout(graceful_timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(ProcessCleanupResult {
            exit_status: status,
            forced_kill: false,
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => {
            let exit_status = kill_process_group(child).await?;
            Ok(ProcessCleanupResult {
                exit_status,
                forced_kill: true,
            })
        }
    }
}

pub async fn kill_process_group(child: &mut AsyncGroupChild) -> io::Result<ExitStatus> {
    // Hit the whole process group, not just the leader.
    #[cfg(unix)]
    {
        if let Some(pid) = child.inner().id() {
            let pgid = getpgid(Some(Pid::from_raw(pid as i32)))
                .map_err(|e| io::Error::other(e.to_string()))?;
            let mut leader_status = None;

            for sig in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGKILL] {
                tracing::info!("Sending {:?} to process group {}", sig, pgid);
                if let Err(e) = killpg(pgid, sig) {
                    tracing::warn!(
                        "Failed to send signal {:?} to process group {}: {}",
                        sig,
                        pgid,
                        e
                    );
                }

                let wait = if sig == Signal::SIGKILL {
                    FORCE_KILL_WAIT_TIMEOUT
                } else {
                    Duration::from_secs(2)
                };
                tracing::info!("Waiting {:?} for process group {} to exit", wait, pgid);
                if wait_for_process_group_exit(child, pgid, wait, &mut leader_status).await? {
                    tracing::info!("Process group {} exited after {:?}", pgid, sig);
                    return leader_status.ok_or_else(|| {
                        io::Error::other("process group exited without a leader status")
                    });
                }
            }

            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process group {pgid} survived SIGKILL"),
            ));
        }
    }

    child.kill().await?;
    wait_for_child_exit(child, FORCE_KILL_WAIT_TIMEOUT).await
}

#[cfg(unix)]
async fn wait_for_process_group_exit(
    child: &mut AsyncGroupChild,
    pgid: Pid,
    timeout: Duration,
    leader_status: &mut Option<ExitStatus>,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if leader_status.is_none() {
            *leader_status = child.inner().try_wait()?;
        }
        if !process_group_exists(pgid)? {
            if leader_status.is_none() {
                *leader_status = Some(child.wait().await?);
            }
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
fn process_group_exists(pgid: Pid) -> io::Result<bool> {
    match kill(Pid::from_raw(-pgid.as_raw()), None) {
        Ok(()) | Err(Errno::EPERM) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(io::Error::other(error.to_string())),
    }
}

async fn wait_for_child_exit(
    child: &mut AsyncGroupChild,
    timeout: Duration,
) -> io::Result<ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for child exit after {}ms",
                timeout.as_millis()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use command_group::AsyncCommandGroup;
    use tokio::{process::Command, time::Duration};

    use super::{ProcessCleanupResult, terminate_process_group};

    fn sleep_command(seconds: u64) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                &format!("Start-Sleep -Seconds {seconds}"),
            ]);
            command
        }

        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args(["-lc", &format!("sleep {seconds}")]);
            command
        }
    }

    async fn terminate_sleep_process(
        seconds: u64,
        graceful_timeout: Duration,
    ) -> ProcessCleanupResult {
        let mut child = sleep_command(seconds)
            .group_spawn()
            .expect("spawn sleep process");

        terminate_process_group(&mut child, graceful_timeout)
            .await
            .expect("cleanup result")
    }

    #[tokio::test]
    async fn terminate_process_group_allows_natural_exit_within_timeout() {
        let result = terminate_sleep_process(1, Duration::from_secs(3)).await;

        assert!(!result.forced_kill);
        assert!(result.exit_status.success());
    }

    #[tokio::test]
    async fn terminate_process_group_force_kills_stubborn_child_after_timeout() {
        let result = terminate_sleep_process(30, Duration::from_millis(100)).await;

        assert!(result.forced_kill);
    }
}
