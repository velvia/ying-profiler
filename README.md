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
* Generation of easy to read flamegraphs

`cargo run --profile bench --features profile-spans --example ying_example`