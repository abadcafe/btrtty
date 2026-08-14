use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/btrtty.plist");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets manifest dir"));
    let plist = manifest.join("assets/btrtty.plist");
    println!(
        "cargo:rustc-link-arg-bin=btrtty=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
