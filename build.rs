//! Stamps the commit being built into the binary, so the status page can say
//! exactly which source is deployed.

use std::process::Command;

fn main() {
    // Rebuild when the checked-out commit changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // A dirty tree means the deployed build matches no commit, which is worth
    // saying out loud rather than quietly reporting the parent.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());

    println!(
        "cargo:rustc-env=GIT_COMMIT={commit}{}",
        if dirty { "-dirty" } else { "" }
    );
}
