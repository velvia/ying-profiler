use std::alloc::{GlobalAlloc, Layout};
use std::fmt::Write;
use std::hint::black_box;
use std::io::{stderr, Write as IoWrite};
use std::panic::resume_unwind;
use std::process::abort;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures::future::join_all;
use moka::sync::Cache;
use rand::distributions::Alphanumeric;
use rand::{rngs::SmallRng, Rng, SeedableRng};
use serial_test::serial;
use ying_profiler::callstack::Measurement;
// use ying_profiler::utils::gen_flamegraph;
use ying_profiler::YingProfiler;

#[cfg(test)]
#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::new(5, 64 * 1024 * 1024 * 1024); // Lower sampling ratio to force our code to be tested more

// Number of allocations to attempt, should be >= 2000 so sampler can work
const NUM_ALLOCS: usize = 4000;

/// Runs `body` on its own thread and fails if it has not finished within `timeout`.
///
/// Every failure mode this suite guards against is a deadlock, and a deadlock inside the global
/// allocator hangs the whole test binary with no output: the harness cannot even say which test
/// was running, because printing allocates.  This turns the subset of those hangs that leave the
/// allocator usable into a normal test failure that names the test.
///
/// It is explicitly best-effort and cannot be the only line of defence.  When the wedged shard is
/// one this thread also needs, the watchdog's own `panic!` allocates and blocks too.  Reintroducing
/// the `dealloc` ordering bug was measured to wedge the process during harness startup, before any
/// of this code runs at all.  The outer layers are the per-test timeout in `.config/nextest.toml`
/// and the step timeouts in CI; see the testing notes in the README.
fn with_watchdog(name: &str, timeout: Duration, body: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = channel();
    let handle = thread::spawn(move || {
        body();
        // Send failing just means the watchdog already gave up; nothing useful to do about it
        let _ = done_tx.send(());
    });

    match done_rx.recv_timeout(timeout) {
        // The sender is dropped without sending only when the body panicked, so in both of these
        // cases re-raise the original panic instead of reporting a deadlock
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            if let Err(panic) = handle.join() {
                resume_unwind(panic)
            }
        }
        Err(RecvTimeoutError::Timeout) => {
            // Deliberately not `panic!`.  Formatting a panic message allocates, and if the reason
            // we got here is a wedged allocator then panicking hangs too, losing the one chance to
            // learn anything.  Writing byte slices straight to fd 2 allocates nothing, and `abort`
            // raises SIGABRT so the OS crash reporter captures a backtrace for *every* thread —
            // the only way to see which lock the hung threads are actually parked on.  On macOS
            // that lands in ~/Library/Logs/DiagnosticReports; on Linux, enable a core dump.
            let mut err = stderr().lock();
            let _ = err.write_all(b"\nWATCHDOG TIMEOUT in test: ");
            let _ = err.write_all(name.as_bytes());
            let _ =
                err.write_all(b"\nlikely allocator deadlock; aborting to dump all thread stacks\n");
            let _ = err.flush();
            abort();
        }
    }
}

/// Allocates from a call stack determined by `path`, one distinct stack per value.
///
/// Growing `stack_stats` and `symbol_map` requires genuinely distinct stack traces, and a loop
/// only ever produces one.  Recursing through two `#[inline(never)]` functions chosen bit by bit
/// from `path` yields 2^depth different traces from a handful of lines of code.
///
/// The two functions must not be tail calls, or the optimizer collapses the whole chain into one
/// frame and every path produces the same trace; the `black_box` after each call is what keeps the
/// frame alive.  They also do slightly different work so the linker cannot fold them together.
fn alloc_from_distinct_stack(depth: u32, path: u32) -> Vec<u8> {
    if depth == 0 {
        return vec![7u8; 256];
    }
    if path & 1 == 0 {
        alloc_via_left(depth - 1, path >> 1)
    } else {
        alloc_via_right(depth - 1, path >> 1)
    }
}

#[inline(never)]
fn alloc_via_left(depth: u32, path: u32) -> Vec<u8> {
    let v = alloc_from_distinct_stack(depth, path);
    black_box(v.len());
    v
}

#[inline(never)]
fn alloc_via_right(depth: u32, path: u32) -> Vec<u8> {
    let v = alloc_from_distinct_stack(depth, path);
    black_box(v.capacity());
    v
}

// NOTE: tests must run serially because allocator is global and we count stats
#[test]
#[serial]
fn basic_allocation_free_test() {
    // We need to give some time for the profiler to start up
    thread::sleep(Duration::from_millis(100));

    // Reset state so mixing tests isn't a problem
    YING_ALLOC.reset_state_for_testing_only();

    // Make thousands of allocations by allocating some small items.  Remember this is a sampling
    // profiler, so we need to make enough.
    let mut items: Vec<_> = (0..NUM_ALLOCS).map(|_n| Box::new([0u64; 64])).collect();

    // Check allocation stats
    let allocated_now = YingProfiler::total_retained_bytes();
    println!("allocated_now = {}", allocated_now);

    let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);
    for s in &top_stacks {
        println!("---\n{}\n", s.rich_report(&YING_ALLOC, false, true));
    }
    assert!(!top_stacks.is_empty());

    // The top stat should be for our allocations
    let stat = &top_stacks[0];
    assert_eq!(stat.freed_bytes, 0);
    let allocated = stat.allocated_bytes;
    assert_eq!(allocated / stat.num_allocations, 512);

    // Now drop some of those items, maybe say half.  The freed stats should update.
    items.truncate(NUM_ALLOCS / 2);
    thread::sleep(Duration::from_millis(100));

    // Check allocation stats - freed bytes should be updated
    let allocated2 = YingProfiler::total_retained_bytes();
    println!("allocated2 = {}", allocated2);
    // After freeing mmeory - less memory should be allocated
    assert!(allocated2 < allocated_now);

    let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);
    assert!(!top_stacks.is_empty());
    let stat = &top_stacks[0];
    println!(
        "\n---xxx after dropping xxx---\n{}",
        stat.rich_report(&YING_ALLOC, false, true)
    );

    // Number of freed bytes should be roughly half
    assert!(stat.freed_bytes > 0);
    assert!(stat.retained_profiled_bytes() > 0);
}

#[test]
#[serial]
fn test_giant_allocation() {
    // We need to give some time for the profiler to start up
    thread::sleep(Duration::from_millis(100));

    // Create an allocation that's way too giant.
    let layout = Layout::from_size_align(128 * 1024 * 1024 * 1024, 8).unwrap();

    // We should get back a null pointer so allocation should fail.
    let ptr = unsafe { YingProfiler::alloc(&YING_ALLOC, layout) };
    assert_eq!(ptr as u64, 0);
}

// Reproduces deadlock produced when we print out stack traces and also insert new symbols at the same time
#[test]
#[serial]
fn test_print_allocations_deadlock() {
    // Make thousands of allocations by allocating some small items.  Remember this is a sampling
    // profiler, so we need to make enough.
    let _items: Vec<_> = (0..NUM_ALLOCS).map(|_n| Box::new([0u64; 64])).collect();

    let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);

    // Reset counter to guarantee next allocation will sample
    println!("before potential deadlock");
    YING_ALLOC.testing_only_guarantee_next_sample();

    for s in &top_stacks {
        // This should generate a bunch of allocations, which should cause potential deadlocks
        println!("---\n{}\n", s.rich_report(&YING_ALLOC, false, false));
    }
}

#[tokio::test]
#[serial]
async fn stress_test() {
    YING_ALLOC.reset_state_for_testing_only();

    // Spin up tons of allocations in a bunch of threads.
    // At the same time, spin up a task which is repeatedly printing alloc reports into a string
    // buffer - thus forcing allocator to be updating and separately reading all the time.
    let dump_allocs_handle = thread::spawn(|| {
        for _ in 0..4000 {
            let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(10);
            // Big growing allocation here for string report
            let mut report_str = String::new();
            for s in &top_stacks {
                writeln!(
                    &mut report_str,
                    "---\n{}\n",
                    s.rich_report(&YING_ALLOC, true, false)
                )
                .unwrap();
            }
        }
        println!("Finished dumping reports...");
    });

    let cache = Cache::new(10_000);

    let num_outer_loops = 50;
    let num_inner_loops = 1000;

    let rng = SmallRng::from_entropy();
    for outer in 0isize..num_outer_loops {
        let starting_num = outer * num_inner_loops;
        let prev_num = (outer - 1) * num_inner_loops;

        let handles: Vec<_> = (0..num_inner_loops)
            .map(|n| {
                let mut rng = rng.clone();
                let cache = cache.clone();
                tokio::task::spawn(async move {
                    let new_str: String =
                        (0..10).map(|_| rng.sample(Alphanumeric) as char).collect();
                    cache.insert(starting_num + n, new_str);
                })
            })
            .collect();

        // At the same time remove a bunch of keys to free up memory - exercise free()
        if prev_num >= 0 {
            for n in 0..1000 {
                cache.invalidate(&(prev_num + n));
            }
        }

        join_all(handles).await;
    }
    println!("Finished alloc/dealloc cycles");

    dump_allocs_handle.join().expect("Cannot wait for thread");

    let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);
    for s in &top_stacks {
        // println!("---\n{}\n", s.rich_report(false));
        println!(
            "{}",
            s.dtrace_report(&YING_ALLOC, Measurement::RetainedBytes)
        );
    }
    assert!(!top_stacks.is_empty());

    // gen_flamegraph(&YING_ALLOC, Measurement::RetainedBytes, &std::path::PathBuf::from("flame.svg")).unwrap();

    // At sampling every 5 allocations, we should have at least outer*inner/5 allocations in the 5 stacks,
    // probably times constant factor of at least 2, plus frees
    // This tests that the sampling is working correctly.  If somehow we stop sampling these numbers
    // would not be so high.
    let total_allocs: u64 = top_stacks.iter().map(|s| s.num_allocations).sum();
    let total_frees: u64 = top_stacks.iter().map(|s| s.num_frees).sum();

    let total_expected_allocs = num_outer_loops * num_inner_loops / 5;
    assert!(total_allocs >= total_expected_allocs as u64);
    assert!(total_frees >= (total_expected_allocs * 9 / 10) as u64); // > 90% of allocs freed
}

/// Hammers the map growth path, which is where the first of the two re-entrancy deadlocks lived.
///
/// A shard that resizes allocates a new table and frees the old one while holding its own write
/// lock, and both of those land back in the allocator.  Thousands of distinct stacks force that to
/// happen over and over in `stack_stats` and `symbol_map`, while a reader thread iterates the same
/// maps.  Run this with `YING_TEST_MAP_CAPACITY=1` to start every map at zero capacity so the
/// resizes begin immediately.
#[test]
#[serial]
fn many_distinct_stacks_while_reporting() {
    with_watchdog(
        "many_distinct_stacks_while_reporting",
        Duration::from_secs(120),
        || {
            YING_ALLOC.reset_state_for_testing_only();

            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = stop.clone();
            let reader = thread::spawn(move || {
                let mut reports = 0u64;
                while !reader_stop.load(Relaxed) {
                    // Iterates every shard of stack_stats, and resolves symbols out of symbol_map
                    // while the writers below are still growing both
                    for s in &YING_ALLOC.top_k_stacks_by_retained(20) {
                        let mut report = String::new();
                        write!(&mut report, "{}", s.rich_report(&YING_ALLOC, true, true)).unwrap();
                    }
                    reports += 1;
                }
                reports
            });

            // 2^10 distinct call stacks per writer thread, allocated and freed repeatedly
            let writers: Vec<_> = (0..4u32)
                .map(|t| {
                    thread::spawn(move || {
                        for round in 0..8u32 {
                            for path in 0..1024u32 {
                                let v = alloc_from_distinct_stack(10, path ^ (t * 7) ^ round);
                                assert_eq!(v.len(), 256);
                            }
                        }
                    })
                })
                .collect();

            for w in writers {
                w.join().expect("writer thread panicked");
            }
            stop.store(true, Relaxed);
            let reports = reader.join().expect("reader thread panicked");

            println!(
                "Generated {reports} reports over {} distinct stacks",
                YING_ALLOC.num_stack_traces()
            );
            // The point of the test is the deadlock, but if the stacks all collapsed to one hash
            // the resize path was never exercised and the test proved nothing
            assert!(
                YING_ALLOC.num_stack_traces() > 100,
                "expected many distinct stacks, got {}",
                YING_ALLOC.num_stack_traces()
            );
        },
    );
}

/// Exercises the thread-local slot cache and the lazy per-thread initialization inside the
/// standard library's parking primitives, which is where the second deadlock lived.
///
/// The `parking_lot_core` deadlock only reproduced once enough threads existed to make that
/// library grow its global parking table, so thread count is the variable that matters here, not
/// allocation volume.  Threads also churn deliberately: `YingLocalCache` hashes slots by thread id,
/// so recycled ids exercise slot reuse.
#[test]
#[serial]
fn many_threads_allocating_and_reporting() {
    with_watchdog(
        "many_threads_allocating_and_reporting",
        Duration::from_secs(120),
        || {
            YING_ALLOC.reset_state_for_testing_only();

            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = stop.clone();
            let reader = thread::spawn(move || {
                while !reader_stop.load(Relaxed) {
                    YING_ALLOC.top_k_stacks_by_allocated(10);
                    YING_ALLOC.num_outstanding_allocs();
                    YING_ALLOC.symbol_map_size();
                }
            });

            // Several waves, so threads are created and destroyed rather than all coexisting
            for wave in 0..4u32 {
                let workers: Vec<_> = (0..48u32)
                    .map(|t| {
                        thread::spawn(move || {
                            // Contend on a shared lock so threads actually park, which is what
                            // triggers the lazy parking-table growth
                            let mut kept = Vec::new();
                            for i in 0..2000u32 {
                                kept.push(vec![0u8; 64 + (i % 97) as usize]);
                                if kept.len() > 64 {
                                    kept.remove(0);
                                }
                                if i % 128 == 0 {
                                    thread::sleep(Duration::from_micros(1));
                                }
                            }
                            alloc_from_distinct_stack(6, t ^ wave).len()
                        })
                    })
                    .collect();
                for w in workers {
                    w.join().expect("worker thread panicked");
                }
            }

            stop.store(true, Relaxed);
            reader.join().expect("reader thread panicked");
        },
    );
}

/// Drives the `realloc` path, which has its own copy of the re-entrancy check and its own
/// outstanding-allocation bookkeeping (remove the old pointer, insert the new one).
///
/// Growing a `Vec` one push at a time is the cheapest way to generate a long chain of reallocs of
/// the same logical allocation, which is exactly what the profiler tries to track as one entry.
#[test]
#[serial]
fn realloc_churn() {
    with_watchdog("realloc_churn", Duration::from_secs(120), || {
        YING_ALLOC.reset_state_for_testing_only();

        let workers: Vec<_> = (0..8u32)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..200 {
                        let mut grown = Vec::new();
                        for i in 0..2000u32 {
                            grown.push(i);
                        }
                        // Shrinking reallocs too
                        grown.truncate(10);
                        grown.shrink_to_fit();
                        assert_eq!(grown.len(), 10);
                    }
                })
            })
            .collect();

        for w in workers {
            w.join().expect("worker thread panicked");
        }

        let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);
        assert!(!top_stacks.is_empty());
    });
}
