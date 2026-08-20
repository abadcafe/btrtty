use clap::Parser;

use crate::cli::Cli;

#[test]
fn test_parse_defaults() {
    let cli = Cli::try_parse_from(["btrtty"]).expect("valid arguments");

    assert_eq!(cli.command, "exec /bin/bash -l");
    assert_eq!((cli.rows, cli.cols), (24, 80));
}

#[test]
fn test_parse_custom_session() {
    let cli = Cli::try_parse_from([
        "btrtty",
        "--command",
        "/opt/homebrew/bin/tmux new-session -A -s btrtty",
        "--rows",
        "51",
        "--cols",
        "177",
    ])
    .expect("valid arguments");

    assert!(cli.command.contains("tmux"));
    assert_eq!((cli.rows, cli.cols), (51, 177));
}

#[test]
fn test_parse_rejects_zero_size() {
    assert!(Cli::try_parse_from(["btrtty", "--rows", "0"]).is_err());
    assert!(Cli::try_parse_from(["btrtty", "--cols", "0"]).is_err());
}
