use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use std::{fs::File, io::Cursor};

use callstack::Measurement;
use derive_builder::Builder;
use inferno::collapse::{dtrace, Collapse};
use inferno::flamegraph;
use log::{error, info};

use super::*;

/// A background thread that dumps out stats, flamegraphs, and does other periodic cleanup.
/// 1. Dumps out top retained memory stats to both logs and disk
/// 2. Can optionally dump out flamegraphs
/// 3. Lets you choose between dumping retained or allocated reports/flamegraphs
///
/// The runner thread will check the amount of retained memory as estimated by this profiler,
/// every `check_interval_secs`.  If the retained memory changes from the previous time by more than
/// `report_pct_change_trigger`, then it will dump out a memory report of the top either retained
/// or allocated stack traces as a file to the chosen `reporting_path` directory on disk.  The file will
/// have the ISO8601 timestamp and the amount of retained memory in the filename for convenience.
/// Optionally, a flamegraph will also be dumped.
///
/// Note that ProfilerRunner uses log framework to periodically dump out logs.  The app is responsible
/// for initializing the logging infrastructure.
///
/// To run:
/// ```
///     use ying_profiler::{YingProfiler, utils::ProfilerRunner};
///     static YING_ALLOC: YingProfiler = YingProfiler::default();
///     ProfilerRunner::default().spawn(&YING_ALLOC);
/// ```
///
/// Builder pattern can also be used.
/// ```
///     use ying_profiler::{YingProfiler, utils::ProfilerRunnerBuilder};
///     static YING_ALLOC: YingProfiler = YingProfiler::default();
///     let runner = ProfilerRunnerBuilder::default()
///         .gen_flamegraphs(true)
///         .reporting_path("profiler_output/")
///         .build()
///         .unwrap();
///      runner.spawn(&YING_ALLOC);
/// ```
#[derive(Clone, Debug, PartialEq, Builder)]
#[builder(setter(into))]
pub struct ProfilerRunner {
    /// Number of seconds in between memory checks
    #[builder(default = "300")]
    check_interval_secs: usize,
    /// Percent change in retained memory to trigger a report
    #[builder(default = "10")]
    report_pct_change_trigger: usize,
    /// Path to write top retained memory reports to
    #[builder(default)]
    reporting_path: String,
    /// Expand inlined call stack symbols with a > ?
    #[builder(default = "false")]
    expand_frames: bool,
    /// Generate flamegraphs at reporting_path
    #[builder(default = "false")]
    gen_flamegraphs: bool,
    /// True=measure allocated memory instead of False=measure retained memory
    #[builder(default = "false")]
    measure_allocated_not_retained: bool,
}

const INITIAL_RETAINED_MEM_MB: usize = 20;

/// Creates a new ProfilerRunner with default values.  Writes reports to current directory, does not expand frames,
/// every 5 minute checks on memory, 10% change triggers report.  No flamegraphs, retained memory.
impl Default for ProfilerRunner {
    fn default() -> Self {
        Self::new(300, 10, "", false, false, false)
    }
}

impl ProfilerRunner {
    /// Creates a new ProfilerRunner with specific parameters
    pub fn new(
        check_interval_secs: usize,
        report_pct_change_trigger: usize,
        reporting_path: &str,
        expand_frames: bool,
        gen_flamegraphs: bool,
        measure_allocated_not_retained: bool,
    ) -> Self {
        Self {
            check_interval_secs,
            report_pct_change_trigger,
            reporting_path: reporting_path.to_string(),
            expand_frames,
            gen_flamegraphs,
            measure_allocated_not_retained,
        }
    }

    /// Spawn a new background thread to run profiler and get stats
    pub fn spawn(&self, profiler: &'static YingProfiler) {
        let check_interval_secs = self.check_interval_secs;
        let report_pct_change_trigger = self.report_pct_change_trigger;
        let reporting_path = PathBuf::from(self.reporting_path.clone());
        let expand_frames = self.expand_frames;
        let profiler2 = profiler;
        let measurement = if self.measure_allocated_not_retained {
            Measurement::AllocatedBytes
        } else {
            Measurement::RetainedBytes
        };
        let gen_flamegraphs = self.gen_flamegraphs;

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

                    let top_stacks = match measurement {
                        Measurement::AllocatedBytes => profiler2.top_k_stacks_by_allocated(10),
                        Measurement::RetainedBytes => profiler2.top_k_stacks_by_retained(10),
                    };
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

                    if gen_flamegraphs {
                        let graph_name = format!("ying.{}.{}MB.svg", dt_str, new_allocated as i64);
                        let mut graph_path = reporting_path.clone();
                        graph_path.push(graph_name);
                        if let Err(e) = gen_flamegraph(profiler2, measurement, &graph_path) {
                            error!("Error generating flame graph to {:?}: {}", &graph_path, e);
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiler_runner_builder() {
        let runner = ProfilerRunnerBuilder::default()
            .gen_flamegraphs(true)
            .build()
            .unwrap();
        assert_eq!(runner.check_interval_secs, 300);
        assert!(runner.gen_flamegraphs);
        assert_eq!(runner.report_pct_change_trigger, 10);
        assert_eq!(runner.reporting_path, "");
    }
}
