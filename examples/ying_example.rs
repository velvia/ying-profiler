//! Long-running example showing Ying's periodic reports and flamegraphs.
//!
//! The original version of this example finished in about a second — far shorter than Ying's
//! default five-minute reporting interval — so the `ying-profiles/` directory stayed empty.
//! This version keeps a toy cache service running long enough for the `ProfilerRunner`
//! background thread to wake up, notice that retained memory has moved, and write a
//! `ying.<timestamp>.<MB>MB.report` plus a `.svg` flamegraph at every check.
//!
//! The workload has two parts, chosen so both of Ying's views have something to say:
//!
//! * **Churn**: every round, a thousand tasks insert a random string into a shared bounded
//!   `moka` cache.  Old entries are evicted and freed, so this stack shows up in the
//!   *allocated* rankings and in the free-time histogram, but not in retained memory.
//! * **Leak**: each round also drops a fresh buffer into a `Vec` that is never freed.
//!   Retained memory climbs steadily, which guarantees it moves more than the 10% change
//!   trigger between checks — so a report is written at *every* check, and the leak
//!   dominates the *retained* rankings.
//!
//! Defaults match `YingProfiler::start_profiling()` — 5 minute checks, 10% change trigger,
//! reports and flamegraphs under `ying-profiles/` — and the run lasts long enough for two
//! checks (about 12 minutes).  Every knob has an env override for shorter demos:
//!
//! ```bash
//! # The default: 5-minutely reports and flamegraphs, run ~12 minutes
//! cargo run --profile bench --example ying_example
//!
//! # A quick ~45 second demo writing a report every 10 seconds
//! YING_EXAMPLE_INTERVAL_SECS=10 YING_EXAMPLE_RUNTIME_SECS=45 \
//!     cargo run --profile bench --example ying_example
//! ```
//!
//! At the end, the top stacks by *total allocated* and by *retained* bytes are both printed,
//! showing how the two rankings differ: the churn stack dominates the first, the leak the
//! second.

use std::env;
use std::time::{Duration, Instant};

use futures::future::join_all;
use moka::sync::Cache;
use rand::distributions::Alphanumeric;
use rand::{rngs::SmallRng, Rng, SeedableRng};
#[cfg(feature = "profile-spans")]
use tracing::instrument;

use ying_profiler::utils::{ProfilerRunnerBuilder, DEFAULT_REPORTING_PATH};
use ying_profiler::YingProfiler;

#[global_allocator]
// Sampling 1 in 5 allocations rather than the default 1 in 500: the demo workload is far too
// small for the default to land on the interesting stacks (eg the once-per-round leak buffer,
// a single allocation among thousands), and the example is only useful if its reports show
// them.  Production apps should keep the default.
static YING_ALLOC: YingProfiler = YingProfiler::new(5, 64 * 1024 * 1024 * 1024);

/// Bounded so old entries get evicted (and freed) as new ones are inserted: that is what
/// makes the insertion stack show up as churn rather than retained memory.
const CACHE_MAX_ENTRIES: u64 = 2_000;
const TASKS_PER_ROUND: u64 = 1_000;
const ROUND_SLEEP: Duration = Duration::from_secs(2);

/// Fresh bytes moved into the never-freed `leak` vec every round.  Scaled with the check
/// interval so retained memory reliably crosses the 10% reporting trigger at every check,
/// whatever interval the example is configured with.
fn leak_bytes_per_round(interval_secs: usize) -> usize {
    (4 * 1024 * 1024 / (interval_secs / 2).max(1)).max(512 * 1024)
}

fn env_val<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    // Sorry this is a dev dependency only, cannot be optional
    tracing_subscriber::fmt::init();

    // Defaults are the same values `YING_ALLOC.start_profiling()` would use: 5 minute checks,
    // 10% change trigger, reports and flamegraphs in `ying-profiles/`, ranked by retained
    // memory.  The builder is used directly so a quick demo can shorten the interval and run
    // length with the YING_EXAMPLE_* env vars.
    let interval_secs: usize = env_val("YING_EXAMPLE_INTERVAL_SECS", 300);
    let runtime_secs: usize = env_val("YING_EXAMPLE_RUNTIME_SECS", interval_secs * 2 + 90);
    let reporting_path: String =
        env::var("YING_EXAMPLE_REPORTING_PATH").unwrap_or_else(|_| DEFAULT_REPORTING_PATH.into());

    let runner = ProfilerRunnerBuilder::default()
        .check_interval_secs(interval_secs)
        .report_pct_change_trigger(10usize)
        .reporting_path(reporting_path)
        .gen_flamegraphs(true)
        .build()
        .unwrap();
    runner.spawn(&YING_ALLOC);

    println!(
        "Ying example running for {runtime_secs}s, checking every {interval_secs}s; \
         a report and flamegraph are written whenever retained memory moves 10%."
    );

    // The leaked buffers are kept alive until after the final stats are printed, so the
    // retained ranking below actually shows them.
    let _leak = cache_update_loop(
        Duration::from_secs(runtime_secs as u64),
        leak_bytes_per_round(interval_secs),
    )
    .await;

    println!("\n===== Done.  Final profiler stats =====");
    println!(
        "Total bytes retained: {}",
        YingProfiler::total_retained_bytes()
    );
    println!(
        "Profiled bytes allocated: {}",
        YingProfiler::profiled_bytes_allocated()
    );
    println!("Size of symbol map: {}", YING_ALLOC.symbol_map_size());

    println!("\n===== Top stacks by TOTAL ALLOCATED (churn should dominate) =====");
    for s in &YING_ALLOC.top_k_stacks_by_allocated(5) {
        println!("---\n{}\n", s.rich_report(&YING_ALLOC, false, false));
    }

    println!("\n===== Top stacks by RETAINED (the leak should dominate) =====");
    for s in &YING_ALLOC.top_k_stacks_by_retained(5) {
        println!("---\n{}\n", s.rich_report(&YING_ALLOC, false, false));
    }
}

/// Runs rounds of insert-churn plus a steady simulated leak until `runtime` has elapsed.
/// Returns the leaked buffers so the caller can keep them alive while reporting.
async fn cache_update_loop(runtime: Duration, leak_bytes: usize) -> Vec<Vec<u8>> {
    let cache: Cache<String, String> = Cache::new(CACHE_MAX_ENTRIES);
    let mut leak: Vec<Vec<u8>> = Vec::new();
    let mut leaked_bytes = 0usize;
    let rng = SmallRng::from_entropy();
    let deadline = Instant::now() + runtime;
    let mut round = 0u64;

    while Instant::now() < deadline {
        round += 1;

        // Churn: TASKS_PER_ROUND inserts into a bounded cache; evictions free old strings.
        let handles: Vec<_> = (0..TASKS_PER_ROUND)
            .map(|_n| {
                let mut rng = rng.clone();
                let cache = cache.clone();
                tokio::task::spawn(async move {
                    let new_str: String = (0..(1024 * (1 + rng.gen_range(0..4))))
                        .map(|_| rng.sample(Alphanumeric) as char)
                        .collect();
                    insert_one(&cache, new_str).await;
                })
            })
            .collect();
        join_all(handles).await;

        // Leak: a fresh buffer every round, never freed.  The call stack is the same every
        // round, so it accumulates into a single stack entry in the retained rankings.
        leak.push(vec![0u8; leak_bytes]);
        leaked_bytes += leak_bytes;

        println!(
            "round {round}: leaked {} MB so far",
            leaked_bytes / (1024 * 1024)
        );
        tokio::time::sleep(ROUND_SLEEP).await;
    }
    leak
}

#[cfg(feature = "profile-spans")]
#[instrument(level = "info", skip_all)]
async fn insert_one(cache: &Cache<String, String>, s: String) {
    cache.insert(s.clone(), s);
}

#[cfg(not(feature = "profile-spans"))]
async fn insert_one(cache: &Cache<String, String>, s: String) {
    cache.insert(s.clone(), s);
}
