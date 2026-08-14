use anyhow::Result;

use super::SessionConfig;

const TERMINAL_TYPE: &str = "xterm-256color";

pub(super) struct PtyProcess {
    pub(super) reader: pty_process::OwnedReadPty,
    pub(super) writer: pty_process::OwnedWritePty,
    pub(super) child: tokio::process::Child,
}

pub(super) fn spawn(config: &SessionConfig) -> Result<PtyProcess> {
    let (pty, pts) = pty_process::open()?;
    pty.resize(pty_process::Size::new(config.rows, config.cols))?;
    let (reader, writer) = pty.into_split();

    let child = pty_process::Command::new("/bin/sh")
        .arg("-lc")
        .arg(&config.command)
        .env("TERM", TERMINAL_TYPE)
        .env("COLORTERM", "truecolor")
        .kill_on_drop(true)
        .spawn(pts)?;

    Ok(PtyProcess {
        reader,
        writer,
        child,
    })
}
