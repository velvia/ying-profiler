/// Utilities for apps using YingProfiler.
/// Most importantly, a ProfilerRunner that spawns a background thread that periodically
/// 1. Dumps out top retained memory stats to both logs and disk
/// 2. ???
///
/// Note that ProfilerRunner uses log framework to periodically dump out logs.  The app is responsible
/// for initializing the logging infrastructure.
///
/// To run:
/// {{{
///     use ying_profiler::utils::ProfilerRunner;
///     ProfilerRunner::default().spawn();
/// }}}
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use log::{error, info};

use super::*;

/// A background thread that dumps out stats and does other periodic cleanup
pub struct ProfilerRunner {
    /// Number of seconds in between memory checks
    check_interval_secs: usize,
    /// Percent change in retained memory to trigger a report
    report_pct_change_trigger: usize,
    /// Path to write top retained memory reports to
    reporting_path: PathBuf,
}

const INITIAL_RETAINED_MEM_MB: usize = 20;

impl ProfilerRunner {
    /// Creates a new ProfilerRunner with specific parameters
    pub fn new(
        check_interval_secs: usize,
        report_pct_change_trigger: usize,
        reporting_path: &str,
    ) -> Self {
        Self {
            check_interval_secs,
            report_pct_change_trigger,
            reporting_path: PathBuf::from(reporting_path),
        }
    }

    /// Creates a new ProfilerRunner with default values
    pub fn default() -> Self {
        Self::new(300, 10, "")
    }

    /// Spawn a new background thread to run profiler and get stats
    pub fn spawn(&self, profiler: &'static YingProfiler) {
        let check_interval_secs = self.check_interval_secs;
        let report_pct_change_trigger = self.report_pct_change_trigger;
        let reporting_path = self.reporting_path.clone();
        let profiler2 = profiler;

        std::thread::spawn(move || {
            let mut last_retained_mem = INITIAL_RETAINED_MEM_MB as f64;

            loop {
                std::thread::sleep(Duration::from_secs(check_interval_secs as u64));

                // Check and compare memory
                let new_allocated = YingProfiler::total_retained_bytes() as f64 / (1024.0 * 1024.0);
                let ratio = (new_allocated - last_retained_mem) / last_retained_mem;

                info!(
                    "Ying: total allocated memory is {:.2} MB and ratio to last = {}",
                    new_allocated, ratio
                );

                // Threshold for change exceeded, do report
                if (ratio.abs() * 100.0) >= report_pct_change_trigger as f64 {
                    #[cfg(feature = "profile-spans")]
                    info!(
                        "Significant memory change registered, dumping profile: new = {}, old = {}",
                        new_allocated, last_retained_mem
                    );

                    last_retained_mem = new_allocated;

                    let top_stacks = profiler2.top_k_stacks_by_retained(10);
                    for s in &top_stacks {
                        // In case the app does not use log, we still output to STDOUT the report
                        println!("---\n{}\n", s.rich_report(profiler2, false));
                    }

                    // Formulate profiling filename based on ISO8601 timestamp and number of MBs
                    let dt = chrono::offset::Local::now();
                    let dt_str = dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let dump_name = format!("ying.{}.{}MB.report", dt_str, new_allocated as i64);

                    let mut report_path = reporting_path.clone();
                    report_path.push(dump_name);
                    if let Ok(f) = File::create(&report_path) {
                        for s in &top_stacks {
                            let _ = writeln!(&f, "---\n{}\n", s.rich_report(profiler2, false));
                        }
                    } else {
                        error!("Error: could not write memory report to {:?}", &report_path);
                    }
                }
            }
        });
    }
}
