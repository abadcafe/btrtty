#![cfg(target_os = "macos")]

use std::process::Command;

fn embedded_plist() -> String {
    let output = Command::new("plutil")
        .arg("-p")
        .arg(env!("CARGO_BIN_EXE_btrtty"))
        .output()
        .expect("run plutil against the btrtty executable");

    assert!(
        output.status.success(),
        "btrtty must contain a readable embedded Info.plist: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("plist output is UTF-8")
}

#[test]
fn test_binary_embeds_bluetooth_usage_description() {
    let plist = embedded_plist();
    assert!(plist.contains("\"NSBluetoothAlwaysUsageDescription\""));
    assert!(plist.contains(
        "\"Btrtty publishes a Bluetooth serial port for terminal clients paired in macOS.\""
    ));
}

#[test]
fn test_binary_embeds_stable_identifier() {
    let plist = embedded_plist();
    assert!(plist.contains("\"CFBundleIdentifier\" => \"dev.btrtty.terminal\""));
}
