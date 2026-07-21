//! Tokio adapter for the engine process boundary.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::ports::process_runner::{EngineProcessOutcome, EngineProcessRunner, EngineProcessSpec};

/// Environment inherited by the engine. The child starts from a cleared
/// environment; job-specific values from `EngineProcessSpec` are added after
/// this allowlist.
const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "LANG",
    "LC_ALL",
    "USER",
    "LOGNAME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "RUST_BACKTRACE",
    "CIH_ASCII",
    "CIH_BULK_BATCH_BYTES",
    "CIH_FALKOR_CONNECT_TIMEOUT_SECS",
    "CIH_FALKOR_LOAD_WAIT_SECS",
    "CIH_PG_URL",
    "POSTGRES_PASSWORD",
    "GRADLE_USER_HOME",
    "HF_HOME",
    "HF_HUB_OFFLINE",
    "CIH_LLM_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
];

#[derive(Clone, Default)]
pub(crate) struct TokioEngineProcessRunner;

#[async_trait]
impl EngineProcessRunner for TokioEngineProcessRunner {
    async fn run(
        &self,
        spec: EngineProcessSpec,
        mut cancel: watch::Receiver<bool>,
    ) -> EngineProcessOutcome {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in ENGINE_ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let program = spec.program.display().to_string();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return EngineProcessOutcome::LaunchFailed(format!(
                    "failed to launch {program}: {error}"
                ))
            }
        };
        let out_task = child
            .stdout
            .take()
            .map(|stream| tokio::spawn(read_capped(stream, spec.output_cap)));
        let err_task = child
            .stderr
            .take()
            .map(|stream| tokio::spawn(read_capped(stream, spec.output_cap)));

        enum Completion {
            Wait(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        let completion = tokio::select! {
            status = child.wait() => Completion::Wait(status),
            _ = tokio::time::sleep(spec.deadline) => Completion::TimedOut,
            _ = wait_cancelled(&mut cancel) => Completion::Cancelled,
        };

        match completion {
            Completion::TimedOut => {
                let _ = child.kill().await;
                drain(out_task).await;
                drain(err_task).await;
                EngineProcessOutcome::TimedOut
            }
            Completion::Cancelled => {
                let _ = child.kill().await;
                drain(out_task).await;
                drain(err_task).await;
                EngineProcessOutcome::Cancelled
            }
            Completion::Wait(Err(error)) => {
                EngineProcessOutcome::LaunchFailed(format!("failed waiting on {program}: {error}"))
            }
            Completion::Wait(Ok(status)) => {
                let (stdout, out_truncated) = collect(out_task).await;
                let (stderr, err_truncated) = collect(err_task).await;
                EngineProcessOutcome::Exited {
                    code: status.code().unwrap_or(-1),
                    success: status.success(),
                    stdout,
                    stderr,
                    truncated: out_truncated || err_truncated,
                }
            }
        }
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn collect(task: Option<tokio::task::JoinHandle<(String, bool)>>) -> (String, bool) {
    match task {
        Some(task) => task.await.unwrap_or_default(),
        None => (String::new(), false),
    }
}

async fn drain(task: Option<tokio::task::JoinHandle<(String, bool)>>) {
    if let Some(task) = task {
        let _ = task.await;
    }
}

async fn read_capped<R>(mut reader: R, cap: usize) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let take = read.min(cap.saturating_sub(retained.len()));
                retained.extend_from_slice(&chunk[..take]);
                truncated |= take < read;
            }
        }
    }
    (String::from_utf8_lossy(&retained).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn shell(script: &str, deadline: Duration, output_cap: usize) -> EngineProcessSpec {
        EngineProcessSpec {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), script.into()],
            current_dir: std::env::current_dir().unwrap(),
            env: Vec::new(),
            deadline,
            output_cap,
        }
    }

    #[tokio::test]
    async fn captures_exit_and_truncates_output() {
        let (_tx, rx) = watch::channel(false);
        let outcome = TokioEngineProcessRunner
            .run(
                shell(
                    "printf '123456'; printf 'abcdef' >&2",
                    Duration::from_secs(5),
                    4,
                ),
                rx,
            )
            .await;
        assert_eq!(
            outcome,
            EngineProcessOutcome::Exited {
                code: 0,
                success: true,
                stdout: "1234".into(),
                stderr: "abcd".into(),
                truncated: true,
            }
        );
    }

    #[tokio::test]
    async fn reports_non_zero_timeout_cancellation_and_launch_failure() {
        let runner = TokioEngineProcessRunner;
        let (_tx, rx) = watch::channel(false);
        assert!(matches!(
            runner
                .run(shell("exit 7", Duration::from_secs(5), 32), rx)
                .await,
            EngineProcessOutcome::Exited {
                code: 7,
                success: false,
                ..
            }
        ));

        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            runner
                .run(shell("sleep 5", Duration::from_millis(30), 32), rx)
                .await,
            EngineProcessOutcome::TimedOut
        );

        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        assert_eq!(
            runner
                .run(shell("sleep 5", Duration::from_secs(5), 32), rx)
                .await,
            EngineProcessOutcome::Cancelled
        );

        let (_tx, rx) = watch::channel(false);
        let mut missing = shell("", Duration::from_secs(1), 32);
        missing.program = PathBuf::from("/definitely/missing/cih-engine");
        assert!(matches!(
            runner.run(missing, rx).await,
            EngineProcessOutcome::LaunchFailed(_)
        ));
    }

    #[tokio::test]
    async fn clears_environment_except_allowlist_and_explicit_values() {
        let (_tx, rx) = watch::channel(false);
        let mut spec = shell(
            "printf '%s|%s' \"${CIH_RUNNER_TEST_SECRET-unset}\" \"${CIH_GRAPH_KEY-unset}\"",
            Duration::from_secs(5),
            128,
        );
        spec.env.push(("CIH_GRAPH_KEY".into(), "graph-a".into()));
        std::env::set_var("CIH_RUNNER_TEST_SECRET", "must-not-leak");
        let outcome = TokioEngineProcessRunner.run(spec, rx).await;
        std::env::remove_var("CIH_RUNNER_TEST_SECRET");
        assert!(matches!(
            outcome,
            EngineProcessOutcome::Exited { stdout, .. } if stdout == "unset|graph-a"
        ));
    }
}
