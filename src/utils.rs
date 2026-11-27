use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use std::{
    fs::{create_dir_all, File},
    io::Cursor,
};

use callstack::Measurement;
use chrono::{offset::Local, SecondsFormat};
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
/// To run.  `no_run` because spawning the reporting thread and creating its output directory are
/// side effects that do not belong in a doctest:
/// ```no_run
///     use ying_profiler::{YingProfiler, utils::ProfilerRunner};
///     static YING_ALLOC: YingProfiler = YingProfiler::default();
///     ProfilerRunner::default().spawn(&YING_ALLOC);
/// ```
///
/// Builder pattern can also be used.
/// ```no_run
///     use ying_profiler::{YingProfiler, utils::ProfilerRunnerBuilder};
///     static YING_ALLOC: YingProfiler = YingProfiler::default();
///     let runner = ProfilerRunnerBuilder::default()
///         .check_interval_secs(60usize)
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
    #[builder(default = "DEFAULT_CHECK_INTERVAL_SECS")]
    check_interval_secs: usize,
    /// Percent change in retained memory to trigger a report
    #[builder(default = "DEFAULT_PCT_CHANGE_TRIGGER")]
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
    /// Number of top stacks to include in generated flamegraphs
    #[builder(default = "DEFAULT_FLAMEGRAPH_TOP_STACKS")]
    flamegraph_top_stacks: usize,
}

const INITIAL_RETAINED_MEM_MB: usize = 20;

/// Directory that [`YingProfiler::start_profiling`](crate::YingProfiler::start_profiling) writes
/// reports and flamegraphs to.
pub const DEFAULT_REPORTING_PATH: &str = "ying-profiles";

/// Default seconds between memory checks.
pub const DEFAULT_CHECK_INTERVAL_SECS: usize = 300;

/// Default percent change in memory that triggers a report.
pub const DEFAULT_PCT_CHANGE_TRIGGER: usize = 10;

/// Default number of top stacks included in generated flamegraphs.
pub const DEFAULT_FLAMEGRAPH_TOP_STACKS: usize = 50;

/// Creates a new ProfilerRunner with default values.  Writes reports to current directory, does not expand frames,
/// every 5 minute checks on memory, 10% change triggers report.  No flamegraphs, retained memory.
impl Default for ProfilerRunner {
    fn default() -> Self {
        Self::new(
            DEFAULT_CHECK_INTERVAL_SECS,
            DEFAULT_PCT_CHANGE_TRIGGER,
            "",
            false,
            false,
            false,
        )
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
            // `new` keeps its existing signature; use the builder to change this.
            flamegraph_top_stacks: DEFAULT_FLAMEGRAPH_TOP_STACKS,
        }
    }

    /// Spawn a new background thread to run profiler and get stats.
    /// Creates `reporting_path` if it does not already exist.
    pub fn spawn(&self, profiler: &'static YingProfiler) {
        if !self.reporting_path.is_empty() {
            if let Err(e) = create_dir_all(&self.reporting_path) {
                error!(
                    "Ying: could not create reporting directory {:?}, reports will not be written: {}",
                    self.reporting_path, e
                );
            }
        }

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
        let flamegraph_options = FlamegraphOptions {
            measurement,
            top_stacks: self.flamegraph_top_stacks,
        };

        thread::spawn(move || {
            let mut last_retained_mem = INITIAL_RETAINED_MEM_MB as f64;

            loop {
                thread::sleep(Duration::from_secs(check_interval_secs as u64));

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
                    let dt = Local::now();
                    let dt_str = dt.to_rfc3339_opts(SecondsFormat::Secs, true);
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
                        error!("Error: could not write memory report to {:?}", report_path);
                    }

                    if gen_flamegraphs {
                        let graph_name = format!("ying.{}.{}MB.svg", dt_str, new_allocated as i64);
                        let mut graph_path = reporting_path.clone();
                        graph_path.push(graph_name);
                        if let Err(e) =
                            gen_flamegraph_with_options(profiler2, flamegraph_options, &graph_path)
                        {
                            error!("Error generating flame graph to {:?}: {}", graph_path, e);
                        }
                    }
                }
            }
        });
    }
}

/// Options controlling flamegraph generation.
///
/// [`gen_flamegraph`] uses the defaults; this struct is the extension point for
/// callers that need a different number of top stacks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlamegraphOptions {
    /// Rank stacks by retained or total allocated bytes.
    pub measurement: Measurement,
    /// Number of top-ranked stacks to include. Must be greater than zero.
    pub top_stacks: usize,
}

impl Default for FlamegraphOptions {
    fn default() -> Self {
        Self {
            measurement: Measurement::RetainedBytes,
            top_stacks: DEFAULT_FLAMEGRAPH_TOP_STACKS,
        }
    }
}

/// Function to produce a FlameGraph file to a specific path.
/// Specify whether to measure retained or allocated bytes, and the path to write flamegraph file to
/// (should probably end in .svg)
///
/// Uses the default top-stack count while keeping the explicit `measurement`.
/// Kept so existing callers compile; use [`gen_flamegraph_with_options`] to
/// change the number of stacks.
pub fn gen_flamegraph(
    profiler: &YingProfiler,
    measurement: Measurement,
    path: &Path,
) -> Result<(), String> {
    gen_flamegraph_with_options(
        profiler,
        FlamegraphOptions {
            measurement,
            ..Default::default()
        },
        path,
    )
}

/// Produces a flamegraph using explicit [`FlamegraphOptions`].
///
/// Returns an error if `options.top_stacks` is zero, or if the output file
/// cannot be created. The latter used to return `Ok(())` silently.
pub fn gen_flamegraph_with_options(
    profiler: &YingProfiler,
    options: FlamegraphOptions,
    path: &Path,
) -> Result<(), String> {
    // Zero stacks would fall through to inferno's generic "No stack counts
    // found" after creating a file; reject it up front instead.
    if options.top_stacks == 0 {
        return Err("top_stacks must be greater than zero".to_string());
    }

    // Generate dtrace-compatible output
    let mut report = String::new();
    let top_stacks = match options.measurement {
        Measurement::RetainedBytes => profiler.top_k_stacks_by_retained(options.top_stacks),
        Measurement::AllocatedBytes => profiler.top_k_stacks_by_allocated(options.top_stacks),
    };
    for s in &top_stacks {
        writeln!(
            &mut report,
            "{}",
            s.dtrace_report(profiler, options.measurement)
        )
        .map_err(|e| e.to_string())?;
    }

    // Fold/collapse output to folded lines
    let mut folder = dtrace::Folder::default();
    let mut folded_buf = Vec::new();
    let folded_out = Cursor::new(&mut folded_buf);
    folder
        .collapse(Cursor::new(report), folded_out)
        .map_err(|e| e.to_string())?;

    write_flamegraph(&folded_buf, path)
}

/// Writes folded stack lines to `path` as an SVG.
///
/// Split out from stack collection so tests can exercise the file and renderer
/// errors without needing a populated profiler.
fn write_flamegraph(folded: &[u8], path: &Path) -> Result<(), String> {
    let file = File::create(path)
        .map_err(|e| format!("failed to create flamegraph file {}: {e}", path.display()))?;
    flamegraph::from_reader(
        &mut flamegraph::Options::default(),
        Cursor::new(folded),
        file,
    )
    .map_err(|e| format!("failed to generate flamegraph {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ying-{name}-{}-{nanos}.svg", std::process::id()))
    }

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
        assert_eq!(runner.flamegraph_top_stacks, DEFAULT_FLAMEGRAPH_TOP_STACKS);
    }

    #[test]
    fn profiler_runner_default_uses_default_flamegraph_top_stacks() {
        assert_eq!(
            ProfilerRunner::default().flamegraph_top_stacks,
            DEFAULT_FLAMEGRAPH_TOP_STACKS
        );
    }

    #[test]
    fn test_profiler_runner_builder_overrides() {
        let runner = ProfilerRunnerBuilder::default()
            .check_interval_secs(60usize)
            .report_pct_change_trigger(25usize)
            .reporting_path("profiler_output/")
            .expand_frames(true)
            .measure_allocated_not_retained(true)
            .flamegraph_top_stacks(75usize)
            .build()
            .unwrap();
        assert_eq!(runner.check_interval_secs, 60);
        assert_eq!(runner.report_pct_change_trigger, 25);
        assert_eq!(runner.reporting_path, "profiler_output/");
        assert!(runner.expand_frames);
        assert!(runner.measure_allocated_not_retained);
        assert!(!runner.gen_flamegraphs);
        assert_eq!(runner.flamegraph_top_stacks, 75);
    }

    #[test]
    fn flamegraph_options_default_to_retained_measurement_and_fifty_stacks() {
        let options = FlamegraphOptions::default();
        assert_eq!(options.measurement, Measurement::RetainedBytes);
        assert_eq!(options.top_stacks, 50);
    }

    #[test]
    fn gen_flamegraph_with_options_rejects_zero_top_stacks() {
        let profiler = YingProfiler::default();
        let path = temp_path("zero-stacks");
        let options = FlamegraphOptions {
            top_stacks: 0,
            ..Default::default()
        };

        let result = gen_flamegraph_with_options(&profiler, options, &path);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("top_stacks"));
        assert!(!path.exists());
    }

    #[test]
    fn write_flamegraph_writes_an_svg() {
        let path = temp_path("valid");
        write_flamegraph(b"main;leaf 10\n", &path).unwrap();

        let svg = fs::read_to_string(&path).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("main"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn gen_flamegraph_with_options_reports_file_creation_error() {
        let profiler = YingProfiler::default();
        let path = temp_path("missing-parent").join("flamegraph.svg");

        let result = gen_flamegraph_with_options(&profiler, FlamegraphOptions::default(), &path);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("create"));
    }
}
