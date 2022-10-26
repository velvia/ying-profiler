use std::time::Duration;

use ying_profiler::YingProfiler;

#[cfg(test)]
#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler;

// Number of allocations to attempt, should be >= 2000 so sampler can work
const NUM_ALLOCS: usize = 4000;

#[test]
fn basic_allocation_free_test() {
    // We need to give some time for the profiler to start up
    std::thread::sleep(Duration::from_millis(100));

    // Make thousands of allocations by allocating some small items.  Remember this is a sampling
    // profiler, so we need to make enough.
    let mut items: Vec<_> = (0..NUM_ALLOCS).map(|_n| Box::new([0u64; 64])).collect();

    // Check allocation stats
    let allocated_now = YingProfiler::total_allocated();
    println!("allocated_now = {}", allocated_now);

    let top_stacks = YingProfiler::top_k_stacks_by_allocated(5);
    for s in &top_stacks {
        println!("---\n{}\n", s.rich_report(false));
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
    let allocated2 = YingProfiler::total_allocated();
    println!("allocated2 = {}", allocated2);
    // After freeing mmeory - less memory should be allocated
    assert!(allocated2 < allocated_now);

    let top_stacks = YingProfiler::top_k_stacks_by_allocated(5);
    assert!(top_stacks.len() >= 1);
    let stat = &top_stacks[0];
    println!(
        "\n---xxx after dropping xxx---\n{}",
        stat.rich_report(false)
    );

    // Number of freed bytes should be roughly half
    assert!(stat.freed_bytes > 0);
}
