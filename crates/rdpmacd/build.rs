//! Embeds Info.plist into the executable so TCC identifies rdpmacd by bundle identifier and the
//! permission prompts show its name. The identifier only stays stable across rebuilds once the
//! binary is code-signed with a certificate (see scripts/sign-dev.sh).
fn main() {
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
