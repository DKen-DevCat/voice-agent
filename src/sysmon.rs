//! Samples the CPU for the window's top edge.

use std::time::Duration;

use sysinfo::System;

/// Reports the average use of the cores (percent) every second, until
/// report returns false.
pub fn run(report: impl Fn(f64) -> bool) {
    let mut sys = System::new();
    sys.refresh_cpu_usage(); // prime the per-core deltas
    loop {
        std::thread::sleep(Duration::from_secs(1));
        sys.refresh_cpu_usage();
        let per: Vec<f64> = sys.cpus().iter().map(|c| c.cpu_usage() as f64).collect();
        if !per.is_empty() && !report(per.iter().sum::<f64>() / per.len() as f64) {
            return;
        }
    }
}
