//! Shared helpers of the benchmark binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Samples `/proc/self/status` every 10 ms and keeps the peaks of `RssAnon` (heap,
/// stacks: memory that the process owns) and `RssFile` (pages of mapped files, shared
/// with the page cache), in KiB.
#[derive(Clone, Default)]
pub struct MemPeak {
    anon: Arc<AtomicU64>,
    file: Arc<AtomicU64>,
}

fn field(status: &str, name: &str) -> u64 {
    status
        .lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl MemPeak {
    /// Start sampling on a thread of its own.
    pub fn start() -> Self {
        let p = MemPeak::default();
        let q = p.clone();
        std::thread::spawn(move || loop {
            q.sample();
            std::thread::sleep(Duration::from_millis(10));
        });
        p
    }

    /// Take one sample now.
    pub fn sample(&self) {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            self.anon
                .fetch_max(field(&s, "RssAnon:"), Ordering::Relaxed);
            self.file
                .fetch_max(field(&s, "RssFile:"), Ordering::Relaxed);
        }
    }

    /// `"anon_kb":N,"file_kb":M` for a JSON line.
    pub fn json(&self) -> String {
        self.sample();
        format!(
            "\"peak_anon_kb\":{},\"peak_file_kb\":{}",
            self.anon.load(Ordering::Relaxed),
            self.file.load(Ordering::Relaxed)
        )
    }
}
