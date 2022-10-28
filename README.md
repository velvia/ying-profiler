# ying-profiler

Ying is a native Rust memory profiler, built to be efficient in production and yield good insight for asynchronous Rust programs.  Ying(應) is Chinese for eagle.

I started this experiment because existing solutions I looked at were either consuming too much resources, or 
wrote profiling files that were too large or too cumbersome to consume, or were very bad at producing useful
stack traces especially for Rust async programs.

Target features:
* Sampling profiler, so it uses little enough resources to be useful in production
* Targets async Rust programs (especially Tokio apps that use tracing for instrumentation), so you know the stack traces will be useful
  - Specific support for tracing spans and finding allocations by span
* Support for detecting leaks or large amounts of allocated memory that has not been freed
  - Tracks realloc() calls as single long-lived allocation
* Generation of easy to read flamegraphs

To see an example which uses Ying and dumps out top stack traces by allocations:

`cargo run --profile bench --features profile-spans --example ying_example`

## How to use

```rust
use ying_profiler::YingProfiler;

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler;
```

TODO: How to dump out profiling info

## Feature Flags

## Roadmap

0.1.0:
- Track span information (need feature XXYY) in stacks
- Track retained memory, including reallocs
- Get top stack traces by total allocation
- Get top traces by retained allocation
- `ProfilerRunner` -- utility to spin up thread to print out stuff every N minutes when total memory usage changes significantly
- Catch and prevent single allocations greater than say 64GB, dump out giant allocation stack trace

Future:
- TODO: env var or struct configuration of profiler parameters
- TODO: Track how long lived allocations are (histogram?)
- TODO: dump out flamegraphs
- TODO: dump out profiles to local files, separate CLI or utility to analyze them?
- TOOD: Ability to regularly trim or reset state, to avoid using up too much memory.  eg., long lived allocations that don't get released should just be removed from the outstanding_allocs map.
- Only keep the top stack traces (say top 500) by various criteria
