//! A memory profiling global allocator.
//! Experimental only right now.
//!
//! Goals:
//! - Use Rust backtrace to properly expose async call stack
//! - Sampled profiling for production efficiency
//! - Tie in to Tokio tracing spans for context richness
//!
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use backtrace::{Backtrace, BacktraceFrame};
use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};

/// Maximum number of stack frames to keep
const DEFAULT_MAX_FRAMES: usize = 20;
/// Allocation sampling ratio.  Eg: 500 means 1 in 500 allocations are sampled.
const DEFAULT_SAMPLING_RATIO: u32 = 500;
/// The number of frames at the top of the stack to skip.  Most of these have to do with
/// backtrace and this profiler infrastructure.  This number needs to be adjusted
/// depending on the implementation.
const TOP_FRAMES_TO_SKIP: usize = 11;

/// A thin, optimized? wrapper around the standard Backtrace
/// We filter out uninteresting stuff like poll()
pub struct Callstack {
    frames: Vec<BacktraceFrame>,
}

impl Callstack {
    /// Creates a new optimized callstack by filtering frames from a full backtrace
    pub fn new() -> Self {
        // TODO: lots of optimization possible, first one is to call Backtrace::new_unresolved()
        // and manually resolve
        let bt = Backtrace::new();
        let frames = cleanup_backtrace(&bt, DEFAULT_MAX_FRAMES);
        Self { frames }
    }

    // TODO: change this to implement Display or something?
    pub fn print_names(&self) {
        for f in &self.frames {
            println!("{:?}", f.symbols()[0].name());
        }
    }
}

// The goal here is to clean up some things from the backtrace:
// - remove backtrace::backtrace frames at the top
// - remove poll() frames
// - only keep a certain number of frames max (limit)
fn cleanup_backtrace(trace: &backtrace::Backtrace, limit: usize) -> Vec<BacktraceFrame> {
    // For now, we take resolved frames only.
    // In the future, we can heavily optimize this: take unresolved frames, use a cache,
    // identify IPs that we have skipped in the past, etc. etc.
    // Use IP list to identify identical places in the code, etc.
    trace
        .frames()
        .iter()
        .skip(TOP_FRAMES_TO_SKIP)
        .filter(|&f| {
            let symbols = f.symbols();
            !symbols.is_empty() && {
                // We should really further filter traces later. Move this logic out
                if let Some(symbol_name) = symbols[0].name() {
                    let demangled = format!("{:?}", symbol_name);
                    !demangled.contains("Future>::poll::")
                } else {
                    false
                }
            }
        })
        .take(limit)
        .cloned()
        .collect()
}

/// Tai is a memory profiling Allocator wrapper.
/// Tai is the Swahili word for an eagle.
pub struct TaiAllocator;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PROFILED: AtomicUsize = AtomicUsize::new(0);

impl TaiAllocator {
    pub fn total_allocated() -> usize {
        ALLOCATED.load(SeqCst)
    }

    pub fn profiled() -> usize {
        PROFILED.load(SeqCst)
    }
}

// NOTE: The creation of state in this TL must NOT allocate. Otherwise it will cause
// the profiler code to go into an endless loop.
thread_local! {
    static PROFILER_TL: RefCell<(bool, SmallRng)> = RefCell::new((false, SmallRng::from_entropy()));
}

unsafe impl GlobalAlloc for TaiAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // NOTE: the code between here and the state.0 = true must be re-entrant
        // and therefore not allocate, otherwise there will be an infinite loop.
        let ret = System.alloc(layout);
        if !ret.is_null() {
            ALLOCATED.fetch_add(layout.size(), SeqCst);

            // Now, sample allocation - if it falls below threshold, then profile
            // Also, we set a ThreadLocal to avoid re-entry: ie the code below might allocate,
            // and we avoid profiling if we are already in the loop below.  Avoids cycles.
            PROFILER_TL.with(|tl_state| {
                // We do the following in two steps because we cannot borrow_mut() twice
                // if profiler code allocates
                if !tl_state.borrow().0 {
                    let mut state = tl_state.borrow_mut();
                    if (state.1.next_u32() % DEFAULT_SAMPLING_RATIO) == 0 {
                        state.0 = true;
                        // This drop is important for re-entry purposes
                        drop(state);

                        // For now, just get and print the allocation.
                        // We just want to see if we can get rich stack traces.
                        PROFILED.fetch_add(1, SeqCst);
                        Callstack::new().print_names();
                        println!("\n---\n");

                        tl_state.borrow_mut().0 = false;
                    }
                }
            })
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        ALLOCATED.fetch_sub(layout.size(), SeqCst);
    }
}
