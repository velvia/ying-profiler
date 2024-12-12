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

```rust
use ying_profiler::YingProfiler;

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::default();
```

The above sets Ying as the global allocator but does not dump out any profiles or stats.  Underneath Ying collects stats and defers to the original System global allocator.

To dump out stats, one can use methods in `YingProfiler`, but the easiest way is to use `ProfilerRunner`, which starts up a background thread, checks memory use stats, and dumps out reports if the change in memory usage exceeds some threshold.  The default checks every 5 minutes and dumps out a report if the retained memory changes by more than 10%.  Options include a path to write out the text reports to, and if inlined stack frames should be expanded in the reports.

```rust
    use ying_profiler::utils::ProfilerRunner;
    ProfilerRunner::default().spawn(&YING_ALLOC);
```

One can also use `ProfilerRunner` to dump out flamegraphs:

```rust
    use ying_profiler::{YingProfiler, utils::ProfilerRunnerBuilder};
    static YING_ALLOC: YingProfiler = YingProfiler::default();
    let runner = ProfilerRunnerBuilder::default()
        .gen_flamegraphs(true)
        .reporting_path("profiler_output/")
        .build()
        .unwrap();
     runner.spawn(&YING_ALLOC);
```

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
- TODO: env var or struct configuration of profiler parameters
- TODO: separate CLI or utility to analyze them?
- TOOD: Ability to regularly trim or reset state, to avoid using up too much memory.  eg., long lived allocations that don't get released should just be removed from the outstanding_allocs map.
- Only keep the top stack traces (say top 500) by various criteria
