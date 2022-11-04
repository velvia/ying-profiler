use moka::sync::Cache;
use rand::distributions::Alphanumeric;
use rand::{rngs::SmallRng, Rng, SeedableRng};
#[cfg(feature = "profile-spans")]
use tracing::instrument;
/// Example of using YingAllocator.  Just creates some strings and inserts them into a hashmap.
///
/// We should see about even split in allocations between cache_update_loop() and insert_one()
///
use ying_profiler::YingProfiler;

#[global_allocator]
static YING_ALLOC: YingProfiler = YingProfiler::default();

#[tokio::main]
async fn main() {
    // Sorry this is a dev dependency only, cannot be optional
    tracing_subscriber::fmt::init();

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
        "\nTotal bytes retained: {}",
        YingProfiler::total_retained_bytes()
    );
    println!(
        "Profiled bytes allocated: {}",
        YingProfiler::profiled_bytes_allocated()
    );
    println!("Size of symbol map: {}", YingProfiler::symbol_map_size());
    // Try changing last param with_filename to true to print out filenames
    let top_stacks = YingProfiler::top_k_stacks_by_allocated(10);
    for s in &top_stacks {
        println!("---\n{}\n", s.rich_report(false));
    }
}

#[cfg(feature = "profile-spans")]
#[instrument(level = "info", skip_all)]
async fn insert_one(cache: &mut Cache<String, String>, s: String) {
    // Another allocation here
    cache.insert(s.clone(), s);
}

#[cfg(not(feature = "profile-spans"))]
async fn insert_one(cache: &mut Cache<String, String>, s: String) {
    // Another allocation here
    cache.insert(s.clone(), s);
}
