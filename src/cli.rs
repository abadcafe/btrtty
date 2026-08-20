use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "btrtty", version, about)]
pub struct Cli {
    /// Command executed inside the PTY through `/bin/sh -lc`.
    #[arg(long, default_value = "exec /bin/bash -l")]
    pub command: String,

    /// Fixed PTY row count for this server.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u16).range(1..))]
    pub rows: u16,

    /// Fixed PTY column count for this server.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u16).range(1..))]
    pub cols: u16,
}
