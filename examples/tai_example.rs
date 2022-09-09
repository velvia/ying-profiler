/// Example of using TaiAllocator.  Just creates some strings and inserts them into a hashmap.
///
/// We should see about even split in allocations between cache_update_loop() and insert_one()
///
use async_backtrace_test::TaiAllocator;
use moka::sync::Cache;
use rand::distributions::Alphanumeric;
use rand::{rngs::SmallRng, Rng, SeedableRng};

#[global_allocator]
static TAI_ALLOC: TaiAllocator = TaiAllocator;

#[tokio::main]
async fn main() {
    cache_update_loop().await;
}

async fn cache_update_loop() {
    let mut cache = Cache::new(10_000);

    let mut rng = SmallRng::from_entropy();
    for _ in 0..50_000 {
        // One allocation here
        let new_str: String = (0..10).map(|_| rng.sample(Alphanumeric) as char).collect();
        insert_one(&mut cache, new_str).await;
    }

    // Dump out how much has been allocated so far
    println!(
        "\nTotal bytes allocated: {}",
        TaiAllocator::total_allocated()
    );
    println!("Total allocations profiled: {}", TaiAllocator::profiled());
}

async fn insert_one(cache: &mut Cache<String, String>, s: String) {
    // Another allocation here
    cache.insert(s.clone(), s);
}
