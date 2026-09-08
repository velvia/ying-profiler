# ying-profiler

Ying is a native Rust sampling memory profiler which tracks retained memory and allocations.  It is designed for production usage in asynchronous Rust programs.  Ying(鷹) is Chinese for eagle.  🦅🦅🦅

I started this experiment because existing solutions I looked at were either consuming too much resources, or
wrote profiling files that were too large or too cumbersome to consume, or were very bad at producing useful
stack traces especially for Rust async programs, or did not support tracking retained memory in its profiling.

Features:
* Sampling profiler, so it uses little enough resources to be useful in production
* Track retained memory, including reallocs, as well as total allocations
* Targets async Rust programs (especially Tokio apps that use tracing for instrumentation), so you know the stack traces will be useful
  - Specific support for tracing spans and finding allocations by span
  - Removes extra `::poll::` lines in the stack trace for clarity
* Support for detecting leaks or large amounts of allocated memory that has not been freed
  - Tracks realloc() calls as single long-lived allocation
* Automatic and easy flamegraph generation
* Allocation lifetime/length histogram
* Track span information (need feature profile_spans) in stacks
* Get top stack traces by total allocation
* Get top traces by retained allocation
* `ProfilerRunner` -- utility to spin up thread to dump out reports and optionally flamegraphs every N minutes when total memory usage changes significantly
* Catch and prevent single allocations greater than say 64GB, dump out giant allocation stack trace

To see an example which uses Ying and dumps out top stack traces by allocations:

`cargo run --profile bench --features profile-spans --example ying_example`

## How to use

Set Ying as the global allocator, then turn on reporting with one line:

```rust
use ying_profiler::YingProfiler;

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::default();

fn main() {
    YING_ALLOC.start_profiling();
    // ... the rest of your program
}
```

There are three layers here, and it is worth knowing which one you are using.

### 1. The allocator: collects, never reports

The `#[global_allocator]` declaration is what makes Ying sample allocations at all.  On its own it is complete and valid: Ying accumulates stats in memory and defers to the System allocator, but writes nothing anywhere.  Nothing is scheduled and no thread is spawned.

### 2. `ProfilerRunner`: the primitive that decides how stats get reported

`ProfilerRunner` is the piece to reach for whenever the defaults do not fit.  It owns every decision about turning collected stats into output — how often to look, how much of a change is worth reporting, what to measure, where output goes, and in what form:

| Setting | Meaning |
|---|---|
| `check_interval_secs` | how often the background thread wakes up to compare memory use |
| `report_pct_change_trigger` | how much retained memory must move before a report is written |
| `reporting_path` | directory for reports and flamegraphs; created if missing |
| `measure_allocated_not_retained` | rank stacks by total allocated bytes instead of retained |
| `gen_flamegraphs` | also write an SVG flamegraph alongside each text report |
| `expand_frames` | expand inlined symbols within each stack frame in reports |

Build one with `ProfilerRunnerBuilder`, which defaults anything you leave out, then hand it the allocator static to start its thread:

```rust
use ying_profiler::{utils::ProfilerRunnerBuilder, YingProfiler};

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::default();

fn main() {
    let runner = ProfilerRunnerBuilder::default()
        .check_interval_secs(60usize)
        .report_pct_change_trigger(5usize)
        .reporting_path("/var/log/ying")
        .gen_flamegraphs(true)
        .build()
        .unwrap();
    runner.spawn(&YING_ALLOC);
}
```

`start_profiling` is not a separate mechanism: it builds exactly one of these with a specific set of values (5 minute interval, 10% trigger, flamegraphs on, retained memory, output to `ying-profiles`) and spawns it.  Anything it can do, a `ProfilerRunner` you build yourself can do too.

### 3. Reading stats directly

You do not need a runner at all if you would rather decide when to look.  `YingProfiler` exposes the stats as data — `top_k_stacks_by_retained`, `top_k_stacks_by_allocated`, `total_retained_bytes` — and `utils::gen_flamegraph` writes a one-off flamegraph on demand.  This is the route to take when reports should be triggered by your own signals, such as an HTTP endpoint or a health check noticing memory growth.

## Implementation notes

Ying is the global allocator, so everything Ying allocates re-enters Ying.  Two rules keep that from
turning into a deadlock, and both matter if you plan to change the code:

1. **The per-thread re-entrancy guard is checked before any profiler state is touched**, in `alloc`,
   `dealloc` and `realloc` alike.  The profiler's maps allocate and free internally (when a shard
   resizes, for instance), and those calls land back in the allocator while a shard lock is held.
   Touching the map first would try to take a lock the same thread already holds.
2. **The profiler's own maps must not use locks that allocate.**  Ying previously used `DashMap`,
   whose `RwLock` is built on `parking_lot_core`; `parking_lot_core` allocates its global parking
   table while holding its internal bucket locks, and that allocation comes back through Ying into
   DashMap, whose lock slow path then re-enters `parking_lot_core` and deadlocks.  No choice of
   allocator for the hash table fixes this.  Ying therefore uses a small sharded map built on
   `std::sync::RwLock`, which never allocates.

## Testing for deadlocks

Every bug this crate has had in the allocator path was a deadlock, and deadlocks are probabilistic:
one green test run means very little.  Two knobs make them reproducible.

`YING_TEST_MAP_CAPACITY` overrides the initial capacity of every profiler map.  Setting it to `1`
starts each map at zero capacity so shards resize constantly, and resizing is the path where a map
allocates and frees while holding its own lock.  With that set, a re-entrancy bug that otherwise
appears once in tens of runs wedges the process immediately:

```bash
YING_TEST_MAP_CAPACITY=1 cargo nextest run --release --profile stress
```

Timeouts must come from outside the process.  Once the allocator is stuck, so is panicking and
printing, so a test cannot report its own hang.  `cargo nextest` runs each test in a separate
process and kills it on timeout (see `.config/nextest.toml`), and CI puts a `timeout-minutes` on
every test step, because a bad enough bug wedges the binary during startup before nextest can apply
a per-test timeout at all.

## Feature Flags

- `profile_spans` - gets the current span ID for recorded stacks.   NOTE: This feature is experimental and does not yet yield useful information.  It also causes a panic when used with `tracing_subscriber` due to a problem with `current_span()` allocating and potentially causing a RefCell `borrow()` to fail.

## Why a new memory profiler?

All profilers are useful and represent amazing work by their authors.
I do have a writeup of different memory profilers [here](https://github.com/velvia/links/blob/main/rust.md#memoryheap-profiling).

Rust as an ecosystem is lacking in good memory profiling tools.  [Bytehound](https://github.com/koute/bytehound)
is quite good but has a large CPU impact and writes out huge profiling files as it measures every allocation.
Jemalloc/Jeprof does sampling, so it's great for production use, but its output is difficult to interpret, and
it does not track retained memory, which is quite critical for debugging memory issues in production.
Both of the above tools are written with generic C/C++ malloc/preload ABI in mind, so the backtraces
that one gets from their use are really limited, especially for profiling Rust binaries built for
an optimized/release target, and especially async code.  The output also often has trouble with mangled symbols.

If we use the backtrace crate and analyze release/bench backtraces in detail, we can see why that is.
The Rust compiler does a good job of inlining function calls - even ones across async/await boundaries -
in release code.  Thus, for a single instruction pointer (IP) in the stack trace, it might correspond to
many different places in the code.  This is from `examples/ying_example.rs`:

```bash
Some(ying_example::insert_one::{{closure}}::h7eddb5f8ebb3289b)
 > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::h7a53098577c44da0)
 > Some(ying_example::cache_update_loop::{{closure}}::h38556c7e7ae06bfa)
 > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hd319a0f603a1d426)
 > Some(ying_example::main::{{closure}}::h33aa63760e836e2f)
 > Some(<core::future::from_generator::GenFuture<T> as core::future::future::Future>::poll::hb2fd3cb904946c24)
```

A generic tool which just examines the IP and tries to figure out a single symbol would miss out on all of the
inlined symbols.  Some tools can expand on symbols, but the results still aren't very good.

## Roadmap

Future:
- Use other global allocators under the hood, namely Jemalloc
- TODO: env var or struct configuration of profiler parameters
- TODO: separate CLI or utility to analyze them?
- TOOD: Ability to regularly trim or reset state, to avoid using up too much memory.  eg., long lived allocations that don't get released should just be removed from the outstanding_allocs map.
- Only keep the top stack traces (say top 500) by various criteria
