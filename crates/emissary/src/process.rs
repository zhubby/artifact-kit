//! Every subprocess belongs to an owned process tree. Windows assignment happens
//! before resuming the suspended process, so descendants cannot escape the job.
use anyhow::{Result, bail};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

pub fn spawn(command: Command) -> std::io::Result<Box<dyn ChildWrapper>> {
    let mut command = CommandWrap::from(command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.spawn()
}
pub async fn wait(
    child: &mut dyn ChildWrapper,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<ExitStatus> {
    tokio::select! {
        result = child.wait() => Ok(result?),
        _ = cancel.cancelled() => { let _ = child.start_kill(); let _ = child.wait().await; bail!("execution canceled"); },
        _ = tokio::time::sleep(timeout) => { let _ = child.start_kill(); let _ = child.wait().await; bail!("execution timed out"); }
    }
}
pub async fn output(
    mut command: Command,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>)> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = spawn(command)?;
    let stdout = child.stdout().take();
    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout {
            stdout.take(4 << 20).read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let result = wait(&mut *child, cancel, timeout).await;
    let bytes = reader.await??;
    Ok((result?, bytes))
}
pub async fn read_log(
    mut reader: impl AsyncRead + Unpin,
    step: &str,
    emit: crate::worker::Emitter,
) -> Result<()> {
    let mut buffer = vec![0; 32 * 1024];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            return Ok(());
        }
        emit.send(crate::worker::Event {
            kind: "log".into(),
            step: step.into(),
            data: buffer[..size].to_vec(),
            ..Default::default()
        })
        .await?;
    }
}
pub async fn probe(mut command: Command, timeout: Duration) -> Result<(ExitStatus, Vec<u8>)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn(command)?;
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let read = |pipe: Option<tokio::process::ChildStdout>| async move {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            pipe.take(65536).read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let out = tokio::spawn(read(stdout));
    let err = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(pipe) = stderr {
            pipe.take(65536).read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let status = wait(&mut *child, &CancellationToken::new(), timeout).await;
    let mut bytes = out.await??;
    bytes.extend(err.await??);
    Ok((status?, bytes))
}
