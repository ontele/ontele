// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! In-memory health sampling behind Settings → Health: compute (process CPU,
//! RSS, active streams/transcodes/recordings), network (request and byte
//! counters fed by the HTTP middleware) and storage (`df` over the data,
//! recordings and library roots). One sample every [`SAMPLE_EVERY`]; the ring
//! holds roughly an hour. Everything is best-effort — a metric that cannot be
//! read on this platform reports zero rather than failing the sampler.

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Incremented by the HTTP middleware for every request / response body byte.
pub static HTTP_REQUESTS: AtomicU64 = AtomicU64::new(0);
pub static HTTP_BYTES_OUT: AtomicU64 = AtomicU64::new(0);

pub const SAMPLE_EVERY: Duration = Duration::from_secs(15);
const RING_LEN: usize = 240; // ~1 hour

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at: u64, // unix seconds
    pub cpu_pct: f64,
    pub rss_mb: f64,
    pub streams: usize,
    pub transcodes: usize,
    pub recordings: usize,
    pub req_per_s: f64,
    pub kb_out_per_s: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub label: String,
    pub path: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Default)]
struct Prev {
    cpu_secs: f64,
    reqs: u64,
    bytes: u64,
    at: Option<Instant>,
}

#[derive(Default)]
pub struct Health {
    ring: Mutex<VecDeque<Sample>>,
    disks: Mutex<Vec<Disk>>,
    prev: Mutex<Prev>,
}

impl Health {
    /// Take one sample. `streams`/`transcodes`/`recordings` come from the
    /// caller so this module needs no handle on the managers.
    pub fn sample(&self, streams: usize, transcodes: usize, recordings: usize) {
        let now = Instant::now();
        let cpu_secs = process_cpu_secs(); // None: no /proc on this platform
        let reqs = HTTP_REQUESTS.load(Ordering::Relaxed);
        let bytes = HTTP_BYTES_OUT.load(Ordering::Relaxed);

        let mut prev = self.prev.lock();
        let dt = prev.at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        let sample = Sample {
            at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            cpu_pct: match cpu_secs {
                Some(cs) if dt > 0.0 && cs >= prev.cpu_secs => {
                    ((cs - prev.cpu_secs) / dt * 100.0 * 10.0).round() / 10.0
                }
                Some(_) => 0.0,
                None => instant_cpu_pct(),
            },
            rss_mb: (process_rss_bytes() as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0,
            streams,
            transcodes,
            recordings,
            req_per_s: if dt > 0.0 { ((reqs - prev.reqs) as f64 / dt * 10.0).round() / 10.0 } else { 0.0 },
            kb_out_per_s: if dt > 0.0 {
                ((bytes - prev.bytes) as f64 / 1024.0 / dt * 10.0).round() / 10.0
            } else {
                0.0
            },
        };
        *prev = Prev { cpu_secs: cpu_secs.unwrap_or(0.0), reqs, bytes, at: Some(now) };
        drop(prev);

        let mut ring = self.ring.lock();
        if ring.len() >= RING_LEN {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    /// Refresh disk usage for the given (label, path) roots.
    pub fn sample_disks(&self, roots: &[(String, String)]) {
        *self.disks.lock() = disk_usage(roots);
    }

    pub fn samples(&self) -> Vec<Sample> {
        self.ring.lock().iter().cloned().collect()
    }

    pub fn disks(&self) -> Vec<Disk> {
        self.disks.lock().clone()
    }
}

/// Cumulative process CPU seconds (user + system); `None` without /proc.
fn process_cpu_secs() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // fields 14/15 (1-based, after the parenthesised comm) are utime/stime in ticks
    let rest = stat.rsplit_once(") ").map(|(_, r)| r)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let (u, s) = (f.get(11)?.parse::<u64>().ok()?, f.get(12)?.parse::<u64>().ok()?);
    Some((u + s) as f64 / 100.0) // USER_HZ is 100 on every mainstream kernel
}

/// Instantaneous %CPU via `ps` — macOS/dev fallback when /proc is absent.
fn instant_cpu_pct() -> f64 {
    ps_field("pcpu").unwrap_or(0.0)
}

/// Current RSS in bytes. Linux: `VmRSS` from `/proc/self/status` (already in
/// kB — page-size independent, unlike statm's page counts on 16K/64K ARM
/// kernels); otherwise `ps -o rss=`.
fn process_rss_bytes() -> u64 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status")
        && let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:"))
        && let Some(kb) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u64>().ok())
    {
        return kb * 1024;
    }
    ps_field("rss").map(|kb| (kb * 1024.0) as u64).unwrap_or(0)
}

fn ps_field(field: &str) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// `df -Pk`, one invocation per root: a root that fails (stale NFS mount,
/// unplugged drive) only loses its own entry — batching would shift every
/// following label onto the wrong stdout line.
fn disk_usage(roots: &[(String, String)]) -> Vec<Disk> {
    roots
        .iter()
        .filter(|(_, p)| !p.is_empty() && std::path::Path::new(p).exists())
        .filter_map(|(label, path)| {
            let out = std::process::Command::new("df").args(["-Pk", path]).output().ok()?;
            if !out.status.success() {
                return None;
            }
            // POSIX -P: header, then exactly one line for the argument:
            // Filesystem 1024-blocks Used Available Capacity Mounted-on
            let text = String::from_utf8_lossy(&out.stdout);
            let f: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
            let total = f.get(1)?.parse::<u64>().ok()? * 1024;
            let free = f.get(3)?.parse::<u64>().ok()? * 1024;
            Some(Disk { label: label.clone(), path: path.clone(), total_bytes: total, free_bytes: free })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_and_orders() {
        let h = Health::default();
        for i in 0..(RING_LEN + 5) {
            h.sample(i, 0, 0);
        }
        let s = h.samples();
        assert_eq!(s.len(), RING_LEN);
        assert_eq!(s.last().unwrap().streams, RING_LEN + 4, "newest kept");
        assert_eq!(s.first().unwrap().streams, 5, "oldest evicted");
    }

    #[test]
    fn rates_derive_from_counter_deltas() {
        let h = Health::default();
        h.sample(0, 0, 0); // establishes prev
        HTTP_REQUESTS.fetch_add(30, Ordering::Relaxed);
        HTTP_BYTES_OUT.fetch_add(1024 * 100, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(60));
        h.sample(0, 0, 0);
        let s = h.samples();
        let last = s.last().unwrap();
        assert!(last.req_per_s > 0.0, "requests since the previous sample show as a rate");
        assert!(last.kb_out_per_s > 0.0);
        assert!(last.rss_mb > 0.0, "rss readable on this platform");
    }

    #[test]
    fn disk_usage_reports_real_mounts() {
        let d = disk_usage(&[("data".into(), "/".into()), ("gone".into(), "/definitely-not-here-xyz".into())]);
        assert_eq!(d.len(), 1, "missing roots are skipped");
        assert_eq!(d[0].label, "data");
        assert!(d[0].total_bytes > 0 && d[0].free_bytes <= d[0].total_bytes);
    }
}
