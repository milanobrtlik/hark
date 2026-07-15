//! Bakes the AcoustID application API key into the build from a local `.env`.
//!
//! The key identifies hark to AcoustID; it is not a user secret, but it is kept
//! out of the repository all the same. A `.env` line `ACOUSTID_API_KEY=...` is
//! surfaced to the crate as the compile-time env var of the same name, which
//! `option_env!` reads. When `.env` is absent or the key is blank, nothing is
//! emitted and the fingerprint fallback compiles to a graceful no-op.

use std::fs;

fn main() {
    // Re-run whenever the key file appears, changes or is removed.
    println!("cargo:rerun-if-changed=.env");

    let Ok(contents) = fs::read_to_string(".env") else {
        return;
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "ACOUSTID_API_KEY" {
            continue;
        }
        // Tolerate `KEY="value"` and surrounding whitespace.
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            println!("cargo:rustc-env=ACOUSTID_API_KEY={value}");
        }
        break;
    }
}
