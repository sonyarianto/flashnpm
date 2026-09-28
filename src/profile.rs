//! Phase timing for profiling installs. Off unless `FLASHNPM_PROFILE=1` —
//! when off, a call site costs one atomic load.
//!
//! Prints `PHASE <label> <ms-since-start>` lines to stderr at each mark.

use std::sync::OnceLock;
use std::time::Instant;

fn start() -> Option<&'static Instant> {
    static START: OnceLock<Option<Instant>> = OnceLock::new();
    START
        .get_or_init(|| {
            if std::env::var("FLASHNPM_PROFILE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("on"))
                .unwrap_or(false)
            {
                Some(Instant::now())
            } else {
                None
            }
        })
        .as_ref()
}

/// Mark a phase boundary. No-op unless profiling is on.
pub fn mark(label: &str) {
    if let Some(t0) = start() {
        eprintln!("PHASE {label} {}ms", t0.elapsed().as_millis());
    }
}
