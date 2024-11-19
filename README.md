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
* Allocation lifetime/length histogram
* Track span information (need feature profile_spans) in stacks
* Get top stack traces by total allocation
* Get top traces by retained allocation
* `ProfilerRunner` -- utility to spin up thread to print out stuff every N minutes when total memory usage changes significantly
* Catch and prevent single allocations greater than say 64GB, dump out giant allocation stack trace

To see an example which uses Ying and dumps out top stack traces by allocations:

`cargo run --profile bench --features profile-spans --example ying_example`

## How to use

```rust
use ying_profiler::YingProfiler;

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::default();
```

The above sets Ying as the global allocator but does not dump out any profiles or stats.  Underneath Ying collects stats and defers to the original System global allocator.

To dump out stats, one can use methods in `YingProfiler`, but the easiest way is to use `ProfilerRunner`, which starts up a background thread, checks memory use stats, and dumps out reports if the change in memory usage exceeds some threshold.  The default checks every 5 minutes and dumps out a report if the retained memory changes by more than 10%.  Options include a path to write out the text reports to, and if inlined stack frames should be expanded in the reports.

```rust
    use ying_profiler::utils::ProfilerRunner;
    ProfilerRunner::default().spawn();
```

## Feature Flags

- `profile_spans` - gets the current span ID for recorded stacks.   NOTE: This feature is experimental and does not yet yield useful information.  It also causes a panic when used with `tracing_subscriber` due to a problem with `current_span()` allocating and potentially causing a RefCell `borrow()` to fail.

## Comparisons to other memory profilers

All profilers are useful and represent amazing work by their authors.  TODO: comparison chart.
I do have a writeup of different memory profilers [here](https://github.com/velvia/links/blob/main/rust.md#memoryheap-profiling).

## Roadmap

Next:
* Generation of easy to read flamegraphs

Future:
- TODO: env var or struct configuration of profiler parameters
- TODO: dump out flamegraphs
- TODO: separate CLI or utility to analyze them?
- TOOD: Ability to regularly trim or reset state, to avoid using up too much memory.  eg., long lived allocations that don't get released should just be removed from the outstanding_allocs map.
- Only keep the top stack traces (say top 500) by various criteria
