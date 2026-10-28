//! Build-time wrapper pin check.
//!
//! The wrapper (`percolator-prog`) is a path dependency, so Cargo cannot pin
//! it by rev. This script checks the checkout Cargo will **actually** build
//! (the `percolator-prog` path in this crate's Cargo.toml) against
//! `PERCOLATOR_PROG_SHA` in `scripts/pins.env`, and fails the build when:
//! * `scripts/pins.env` is missing or has no `PERCOLATOR_PROG_SHA`;
//! * the checkout is not a git repo, or `HEAD` differs from the pin;
//! * the checkout has uncommitted changes to tracked files;
//! * `PERCOLATOR_PROG_DIR` is set and names a different directory than the
//!   one Cargo builds (the scripts use it; a stale value must not hide a
//!   mismatch).
//!
//! `PERCOLATOR_SKIP_PIN_CHECK=1` downgrades every failure to a warning. Only
//! the pre-merge preview script sets it, deliberately.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PERCOLATOR_PROG_DIR");
    println!("cargo:rerun-if-env-changed=PERCOLATOR_SKIP_PIN_CHECK");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let skip = std::env::var("PERCOLATOR_SKIP_PIN_CHECK").as_deref() == Ok("1");
    if let Err(msg) = check() {
        if skip {
            println!("cargo:warning=wrapper pin check skipped (PERCOLATOR_SKIP_PIN_CHECK=1): {msg}");
        } else {
            panic!(
                "wrapper pin check failed: {msg}\nRun ./scripts/fetch_upstream.sh, or set \
                 PERCOLATOR_SKIP_PIN_CHECK=1 for a deliberate preview build."
            );
        }
    }
}

fn check() -> Result<(), String> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let pins = find_up(&manifest_dir, "scripts/pins.env")
        .ok_or("scripts/pins.env not found above the crate")?;
    println!("cargo:rerun-if-changed={}", pins.display());
    let want = read_pin(&pins, "PERCOLATOR_PROG_SHA")
        .ok_or_else(|| format!("no PERCOLATOR_PROG_SHA in {}", pins.display()))?;

    // The directory Cargo builds, from this crate's manifest.
    let rel = prog_path_from_manifest(&manifest_dir)
        .ok_or("no `percolator-prog = { path = ... }` dependency in Cargo.toml")?;
    let built = manifest_dir
        .join(&rel)
        .canonicalize()
        .map_err(|e| format!("wrapper path {rel}: {e}"))?;
    if let Ok(env_dir) = std::env::var("PERCOLATOR_PROG_DIR") {
        let env_dir = PathBuf::from(&env_dir)
            .canonicalize()
            .map_err(|e| format!("PERCOLATOR_PROG_DIR={env_dir}: {e}"))?;
        if env_dir != built {
            return Err(format!(
                "PERCOLATOR_PROG_DIR={} but Cargo builds {}",
                env_dir.display(),
                built.display()
            ));
        }
    }

    let git = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&built)
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(format!("{} is not a git checkout", built.display()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Re-run when the checkout moves, its index changes, or any wrapper
    // source/manifest changes (a directory is scanned recursively), so an
    // unstaged edit followed by an incremental build re-runs the dirty check
    // instead of reusing an earlier pass.
    let git_dir = PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?);
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    // Every tracked file, matching the dirty check below (which rejects
    // changes to any tracked file).
    for tracked in git(&["ls-files"])?.lines() {
        println!("cargo:rerun-if-changed={}", built.join(tracked).display());
    }

    let head = git(&["rev-parse", "HEAD"])?;
    if head != want {
        return Err(format!(
            "{} is at {head}, scripts/pins.env pins {want}",
            built.display()
        ));
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "{} has uncommitted changes to tracked files:\n{dirty}",
            built.display()
        ));
    }
    Ok(())
}

fn find_up(start: &Path, rel: &str) -> Option<PathBuf> {
    start.ancestors().map(|d| d.join(rel)).find(|p| p.is_file())
}

fn read_pin(path: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

fn prog_path_from_manifest(manifest_dir: &Path) -> Option<String> {
    let toml = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).ok()?;
    toml.lines()
        .find(|l| l.trim_start().starts_with("percolator-prog"))
        .and_then(|l| l.split("path = \"").nth(1))
        .and_then(|r| r.split('"').next())
        .map(str::to_string)
}
