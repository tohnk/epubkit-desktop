//! The command line's options, checked before anything is done with them.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// JPEG quality is 20 to 95, as the CLI's help says and the app's slider
/// allows. Any byte was taken, and remembered for every run after it:
/// `--quality 9`, for 90, made each book from then on at the worst quality the
/// encoder has, with nothing said.
#[test]
fn a_quality_outside_its_range_is_refused_and_not_remembered() {
    let home = tempfile::tempdir().unwrap();
    for quality in ["0", "9", "19", "96", "100"] {
        let output = Command::new(env!("CARGO_BIN_EXE_epubkit"))
            .args(["optimize", "missing.epub", "--quality", quality])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "--quality {quality}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains("20..=95"), "--quality {quality}: {said}");
    }
    assert_eq!(files_under(home.path()), Vec::<PathBuf>::new());
}
