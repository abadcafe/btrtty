use clap::Parser;

use crate::cli::Cli;

#[test]
fn test_parse_defaults() {
    let cli = Cli::try_parse_from(["btrtty"]).expect("valid arguments");

    assert_eq!(cli.command, "exec /bin/bash -l");
    assert_eq!((cli.rows, cli.cols), (24, 80));
    assert_eq!(cli.max_output_rate, 20);
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
        "--max-output-rate",
        "200",
    ])
    .expect("valid arguments");

    assert!(cli.command.contains("tmux"));
    assert_eq!((cli.rows, cli.cols), (51, 177));
    assert_eq!(cli.max_output_rate, 200);
}

#[test]
fn test_parse_rejects_zero_size() {
    assert!(Cli::try_parse_from(["btrtty", "--rows", "0"]).is_err());
    assert!(Cli::try_parse_from(["btrtty", "--cols", "0"]).is_err());
}

#[test]
fn test_parse_rejects_output_rate_outside_supported_range() {
    assert!(Cli::try_parse_from(["btrtty", "--max-output-rate", "0"]).is_err());
    assert!(Cli::try_parse_from(["btrtty", "--max-output-rate", "201"]).is_err());
}
