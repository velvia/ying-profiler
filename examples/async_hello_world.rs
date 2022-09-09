use async_backtrace_test::Callstack;
use std::time::Duration;
/// Example async hello world showing that one can obtain an async back trace
/// You should see output with blow_up(), bar(), and foo() in the trace
///
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    foo().await;
}

async fn foo() {
    sleep(Duration::from_millis(100)).await;
    bar().await;
}

async fn bar() {
    sleep(Duration::from_millis(200)).await;
    blow_up().await;
}

async fn blow_up() {
    sleep(Duration::from_millis(300)).await;
    println!("Cleaned up backtrace:");
    Callstack::new().print_names();
}
