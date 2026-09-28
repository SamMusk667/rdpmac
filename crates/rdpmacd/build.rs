//! Embeds Info.plist into the executable so TCC identifies rdpmacd by bundle identifier and the
//! permission prompts show its name. The identifier only stays stable across rebuilds once the
//! binary is code-signed with a certificate (see scripts/sign-dev.sh).
//!
//! Also sets RDPMAC_VERSION, the version rdpmacd reports: the package version when the commit
//! built is the one tagged with it (v0.4.0), otherwise the package version, `-dev` and the number
//! of commits the build is made from, such as 0.4.0-dev55.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plist = manifest.join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
    println!("cargo:rustc-env=RDPMAC_VERSION={}", version(manifest));
}

/// The output of git run in `dir`; `None` without git or outside a repository.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn version(dir: &Path) -> String {
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    // A source archive has no history to count.
    let Some(git_dir) = git(dir, &["rev-parse", "--absolute-git-dir"]) else {
        return base;
    };
    // HEAD changes on a checkout, logs/HEAD with every commit; both make the count stale.
    for file in ["HEAD", "logs/HEAD"] {
        let path = Path::new(&git_dir).join(file);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if git(dir, &["describe", "--tags", "--exact-match", "HEAD"]).as_deref() == Some(format!("v{base}").as_str()) {
        return base;
    }
    match git(dir, &["rev-list", "--count", "HEAD"]) {
        Some(count) => format!("{base}-dev{count}"),
        None => base,
    }
}
