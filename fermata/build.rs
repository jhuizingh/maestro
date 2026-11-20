//! Stamp the binary with the git commit it was built from and the build time (see DESIGN.md,
//! "Build identity").
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=FERMATA_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // HEAD moves on checkout; the branch ref (or packed-refs) moves on commit.
    for p in ["HEAD", "packed-refs"] {
        if let Some(f) = git(
            &dir,
            &["rev-parse", "--path-format=absolute", "--git-path", p],
        ) {
            println!("cargo:rerun-if-changed={f}");
        }
    }
    if let Some(r) = git(&dir, &["symbolic-ref", "-q", "HEAD"])
        && let Some(f) = git(
            &dir,
            &["rev-parse", "--path-format=absolute", "--git-path", &r],
        )
    {
        println!("cargo:rerun-if-changed={f}");
    }

    let commit = std::env::var("FERMATA_BUILD_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            let sha = git(&dir, &["rev-parse", "--short=12", "HEAD"])?;
            let dirty =
                git(&dir, &["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty());
            Some(if dirty { format!("{sha}-dirty") } else { sha })
        })
        .unwrap_or_else(|| "unknown".into());
    let when = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .unwrap_or_else(chrono::Utc::now);
    println!("cargo:rustc-env=FERMATA_COMMIT={commit}");
    println!(
        "cargo:rustc-env=FERMATA_BUILT={}",
        when.format("%Y-%m-%d %H:%M UTC")
    );
}
