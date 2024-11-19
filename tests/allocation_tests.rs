use std::alloc::GlobalAlloc;
use std::fmt::Write;
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

// NOTE: tests must run serially because allocator is global and we count stats
#[test]
#[serial]
fn basic_allocation_free_test() {
    // We need to give some time for the profiler to start up
    std::thread::sleep(Duration::from_millis(100));

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
    assert!(top_stacks.len() >= 1);

    // The top stat should be for our allocations
    let stat = &top_stacks[0];
    assert_eq!(stat.freed_bytes, 0);
    let allocated = stat.allocated_bytes;
    assert_eq!(allocated / stat.num_allocations, 512);

    // Now drop some of those items, maybe say half.  The freed stats should update.
    items.truncate(NUM_ALLOCS / 2);
    std::thread::sleep(Duration::from_millis(100));

    // Check allocation stats - freed bytes should be updated
    let allocated2 = YingProfiler::total_retained_bytes();
    println!("allocated2 = {}", allocated2);
    // After freeing mmeory - less memory should be allocated
    assert!(allocated2 < allocated_now);

    let top_stacks = YING_ALLOC.top_k_stacks_by_allocated(5);
    assert!(top_stacks.len() >= 1);
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
    std::thread::sleep(Duration::from_millis(100));

    // Create an allocation that's way too giant.
    let layout = std::alloc::Layout::from_size_align(128 * 1024 * 1024 * 1024, 8).unwrap();

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
    let dump_allocs_handle = std::thread::spawn(|| {
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
    assert!(top_stacks.len() >= 1);

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
