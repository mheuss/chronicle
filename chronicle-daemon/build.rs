use std::env;
use std::fs;
use std::path::PathBuf;

const MIC_USAGE: &str = "Chronicle records microphone audio, when you turn the mic on, so you can search what was said.";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CHRONICLE_BUILD_NUMBER");

    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let build = env::var("CHRONICLE_BUILD_NUMBER").unwrap_or_else(|_| "0".to_string());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>dev.heuss.chronicle.daemon</string>
    <key>CFBundleName</key>
    <string>chronicle-daemon</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{build}</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>{MIC_USAGE}</string>
</dict>
</plist>
"#
    );

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("Info.plist");
    fs::write(&out, plist).expect("write Info.plist to OUT_DIR");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        out.display()
    );
}
