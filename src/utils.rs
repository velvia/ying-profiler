use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;
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
use std::{fs::File, io::Cursor};

use callstack::Measurement;
use inferno::collapse::{dtrace, Collapse};
use inferno::flamegraph;
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
    /// Expand inlined call stack symbols with a > ?
    expand_frames: bool,
}

const INITIAL_RETAINED_MEM_MB: usize = 20;

/// Creates a new ProfilerRunner with default values.  Writes reports to current directory, does not expand frames,
/// every 5 minute checks on memory, 10% change triggers report.
impl Default for ProfilerRunner {
    fn default() -> Self {
        Self::new(300, 10, "", false)
    }
}

impl ProfilerRunner {
    /// Creates a new ProfilerRunner with specific parameters
    pub fn new(
        check_interval_secs: usize,
        report_pct_change_trigger: usize,
        reporting_path: &str,
        expand_frames: bool,
    ) -> Self {
        Self {
            check_interval_secs,
            report_pct_change_trigger,
            reporting_path: PathBuf::from(reporting_path),
            expand_frames,
        }
    }

    /// Spawn a new background thread to run profiler and get stats
    pub fn spawn(&self, profiler: &'static YingProfiler) {
        let check_interval_secs = self.check_interval_secs;
        let report_pct_change_trigger = self.report_pct_change_trigger;
        let reporting_path = self.reporting_path.clone();
        let expand_frames = self.expand_frames;
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
                        println!("---\n{}\n", s.rich_report(profiler2, false, expand_frames));
                    }

                    // Formulate profiling filename based on ISO8601 timestamp and number of MBs
                    let dt = chrono::offset::Local::now();
                    let dt_str = dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let dump_name = format!("ying.{}.{}MB.report", dt_str, new_allocated as i64);

                    let mut report_path = reporting_path.clone();
                    report_path.push(dump_name);
                    if let Ok(f) = File::create(&report_path) {
                        for s in &top_stacks {
                            let _ = writeln!(
                                &f,
                                "---\n{}\n",
                                s.rich_report(profiler2, false, expand_frames)
                            );
                        }
                    } else {
                        error!("Error: could not write memory report to {:?}", &report_path);
                    }
                }
            }
        });
    }
}

/// Function to produce a FlameGraph file to a specific path.
/// Specify whether to measure retained or allocated bytes, and the path to write flamegraph file to
/// (should probably end in .svg)
pub fn gen_flamegraph(
    profiler: &YingProfiler,
    measurement: Measurement,
    path: &PathBuf,
) -> Result<(), String> {
    // Generate dtrace-compatible output
    let mut report = String::new();
    match measurement {
        Measurement::RetainedBytes => {
            let top_stacks = profiler.top_k_stacks_by_retained(50);
            for s in &top_stacks {
                writeln!(
                    &mut report,
                    "{}",
                    s.dtrace_report(profiler, Measurement::RetainedBytes)
                )
                .map_err(|e| e.to_string())?;
            }
        }
        Measurement::AllocatedBytes => {
            let top_stacks = profiler.top_k_stacks_by_allocated(50);
            for s in &top_stacks {
                writeln!(
                    &mut report,
                    "{}",
                    s.dtrace_report(profiler, Measurement::AllocatedBytes)
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    // Fold/collapse output to folded lines
    let mut folder = dtrace::Folder::default();
    let mut folded_buf = Vec::new();
    let folded_out = Cursor::new(&mut folded_buf);
    folder
        .collapse(Cursor::new(report), folded_out)
        .map_err(|e| e.to_string())?;

    // Now, generate the flamegraph from folded lines
    if let Ok(f) = File::create(path) {
        flamegraph::from_reader(
            &mut flamegraph::Options::default(),
            Cursor::new(&folded_buf),
            f,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
