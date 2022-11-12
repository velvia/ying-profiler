use std::borrow::Cow;
use std::fmt;
use std::hash::Hasher;

use backtrace::BacktraceSymbol;
use leapfrog::Value;
use once_cell::sync::Lazy;
use regex::Regex;
use wyhash::WyHash;

use super::*;

pub(crate) const MAX_NUM_FRAMES: usize = 30;

pub type StdCallstack = Callstack<MAX_NUM_FRAMES>;

/// An optimized Callstack struct that represents a single stack trace.
/// No symbols are explicitly held here - the major savings is that
/// we use an external dictionary to store symbols, because the same IPs
/// are used over and over in many stack traces.
///
/// To reduce allocations, we only keep MAX_NUM_FRAMES frames.
#[derive(Debug, Clone, Copy)]
pub struct Callstack<const NF: usize> {
    frames: [u64; NF],
}

impl<const NF: usize> Callstack<NF> {
    /// Creates a Callback from a backtrace::Backtrace, preferably unresolved for speed
    pub fn from_backtrace_unresolved(bt: &backtrace::Backtrace) -> Self {
        let mut cb = Self { frames: [0; NF] };
        for i in TOP_FRAMES_TO_SKIP..(bt.frames().len().min(NF)) {
            cb.frames[i - TOP_FRAMES_TO_SKIP] = bt.frames()[i].ip() as u64;
        }
        cb
    }

    // Used only to create special marker callstacks that are not real, probably for LeapMap
    pub(crate) fn xxx_magic_marker(marker: u64) -> Self {
        Callstack { frames: [marker; NF] }
    }

    pub(crate) fn is_frame_magic(&self, marker: u64) -> bool {
        self.frames[0] == marker
    }

    pub fn compute_hash(&self) -> u64 {
        let mut hasher = WyHash::with_seed(17);
        hasher.write(unsafe { (self.frames).align_to::<u8>().1 });
        hasher.finish() & 0x7fff_ffff_ffff_ffff  // mask out upper bit to prevent clashing with MAX
    }

    /// Goes through the IPs stored and ensures that the symbol map has resolved symbols for
    /// all of them.  If it does not, resolves the backtrace symbols and updates the symbol map.
    /// Potentially very expensive due to resolving IPs
    pub fn populate_symbol_map(&self, bt: &mut backtrace::Backtrace, symbol_map: &SymbolMap) {
        // For each IP in our trace that is not zero
        for (i, ip) in self.frames.iter().enumerate() {
            if *ip == 0 {
                break;
            }

            let mut map = symbol_map.lock().unwrap();
            if !map.contains_key(ip) {
                // IP not there. Get the corresponding frame from the backtrace
                let frame = &bt.frames()[i + TOP_FRAMES_TO_SKIP];

                // Get the symbol out.  Resolve the backtrace if necessary
                if frame.symbols().is_empty() {
                    bt.resolve();
                }
                let frame = &bt.frames()[i + TOP_FRAMES_TO_SKIP];

                // Convert frame symbols into FriendlySymbols and add to symbol map
                let friendlies = frame.symbols().iter().map(FriendlySymbol::from).collect();
                map.insert(*ip, friendlies);
            }
        }
    }

    /// Obtains a DecoratedCallstack for display.
    /// `println!("{}", cb.with_symbols(symbols));`
    pub fn with_symbols<'s, 'm>(
        &'s self,
        symbols: &'m SymbolMap,
    ) -> DecoratedCallstack<'s, 'm, NF> {
        DecoratedCallstack {
            cb: self,
            symbols,
            filename_info: false,
            filter_poll: true,
        }
    }

    /// Obtains a DecoratedCallstack for display with both symbol and filename/lineno info.
    /// `println!("{}", cb.with_symbols_and_filename(symbols));`
    pub fn with_symbols_and_filename<'s, 'm>(
        &'s self,
        symbols: &'m SymbolMap,
    ) -> DecoratedCallstack<'s, 'm, NF> {
        DecoratedCallstack {
            cb: self,
            symbols,
            filename_info: true,
            filter_poll: true,
        }
    }
}

pub struct DecoratedCallstack<'cb, 's, const NF: usize> {
    cb: &'cb Callstack<NF>,
    symbols: &'s SymbolMap,
    filename_info: bool,
    filter_poll: bool,
}

impl<'cb, 's, const NF: usize> fmt::Display for DecoratedCallstack<'cb, 's, NF> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Callback <hash = 0x{:0x}>", self.cb.compute_hash())?;
        for ip in &self.cb.frames {
            let map = self.symbols.lock().unwrap();
            if let Some(symbols) = map.get(ip) {
                if !symbols.is_empty() {
                    writeln!(f, "  {}", stringify_symbol(&symbols[0], self.filename_info))?;
                    // Don't expand inlined `::poll::` subcalls, they aren't interesting
                    if !symbols[0].is_poll {
                        for s in &symbols[1..] {
                            if self.filter_poll && s.is_poll {
                                continue;
                            }
                            writeln!(f, "    > {}", stringify_symbol(s, self.filename_info))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn stringify_symbol(s: &FriendlySymbol, include_filename: bool) -> String {
    if include_filename {
        format!(
            "{}\n\t({:?}:{})",
            s.friendly_name, s.shorter_filename, s.line_no
        )
    } else {
        s.friendly_name.to_string()
    }
}

struct SymbolRegexes {
    name_end_re: Regex,
    // List of common patterns in filenames that can be shortened
    filename_res: Vec<(Regex, &'static str)>,
}

static SYMBOL_REGEXES: Lazy<SymbolRegexes> = Lazy::new(|| {
    let filename_res = vec![
        (Regex::new(r"^/rustc/\w+/library/").unwrap(), "RUST:"),
        (
            Regex::new(r"^/Users/\w+/.cargo/registry/src/github.com-\w+/").unwrap(),
            "Cargo:",
        ),
        (
            Regex::new(r"^/home/\w+/.cargo/registry/src/github.com-\w+/").unwrap(),
            "Cargo:",
        ),
    ];
    SymbolRegexes {
        name_end_re: Regex::new(r"(::\w+)$").expect("Error constructing regex"),
        filename_res,
    }
});

/// A wrapper around BacktraceSymbol with cleaned up, demangled symbol names
/// and shortened filename and line number as well.
///
/// The shorter filename has common patterns like /Users/*/.cargo/registry/src/github.com-..../
/// and /rustc/..../library substituted out for better readability.
pub struct FriendlySymbol {
    friendly_name: String,
    is_poll: bool,
    shorter_filename: String,
    line_no: u32,
}

impl From<&BacktraceSymbol> for FriendlySymbol {
    fn from(s: &BacktraceSymbol) -> Self {
        // Get demangled name and strip the final ::<hex>
        let demangled = format!("{}", s.name().expect("Name should exist"));
        let friendly_name = SYMBOL_REGEXES
            .name_end_re
            .replace(&demangled, "")
            .into_owned();

        let is_poll = demangled.contains("::poll::");

        // Get filename and convert common patterns
        let shorter_filename = if let Some(p) = s.filename() {
            let filename = p.to_str().expect("Cannot convert path to string");
            let mut new_filename = None;
            for (re, abbrev) in &SYMBOL_REGEXES.filename_res {
                let replaced = re.replace(filename, *abbrev);
                if let Cow::Owned(_) = replaced {
                    // This means string was replaced
                    new_filename = Some(replaced.into_owned());
                    break;
                }
            }
            new_filename.unwrap_or_else(|| filename.to_owned())
        } else {
            String::new()
        };

        let line_no = s.lineno().unwrap_or(0);

        Self {
            friendly_name,
            is_poll,
            shorter_filename,
            line_no,
        }
    }
}


/// Central struct collecting stats about each stack trace
#[derive(Debug, Clone, Copy)]
pub struct StackStats {
    stack: StdCallstack,
    pub allocated_bytes: u64,
    pub num_allocations: u64,
    pub freed_bytes: u64,
    pub num_frees: u64,
    #[cfg(feature = "profile-spans")]
    span: tracing::Span,
}

impl Value for StackStats {
    fn is_redirect(&self) -> bool {
        self.stack.is_frame_magic(u64::MAX - 1)
    }

    fn is_null(&self) -> bool {
        self.stack.is_frame_magic(u64::MAX)
    }

    fn redirect() -> Self {
        StackStats::magic_for_leapmap_value(u64::MAX - 1)
    }

    fn null() -> Self {
        StackStats::magic_for_leapmap_value(u64::MAX)
    }
}

impl StackStats {
    // Constructor not public.  Only this crate should create new stats.
    pub(crate) fn new(stack: StdCallstack, initial_alloc_bytes: Option<u64>) -> Self {
        Self {
            stack,
            allocated_bytes: initial_alloc_bytes.unwrap_or(0),
            num_allocations: initial_alloc_bytes.map(|_| 1).unwrap_or(0),
            freed_bytes: 0,
            num_frees: 0,
            #[cfg(feature = "profile-spans")]
            span: tracing::Span::current(),
        }
    }

    fn magic_for_leapmap_value(marker: u64) -> Self {
        Self {
            stack: Callstack::xxx_magic_marker(marker),
            allocated_bytes: 0,
            num_allocations: 0,
            freed_bytes: 0,
            num_frees: 0,
            #[cfg(feature = "profile-spans")]
            span: tracing::Span::none(),
        }
    }

    /// The number of "retained" bytes as seen by this profiler from sampling
    pub fn retained_profiled_bytes(&self) -> u64 {
        // NOTE: saturating_sub here is really important, freed could be slightly bigger than allocated
        self.allocated_bytes.saturating_sub(self.freed_bytes)
    }

    /// Create a rich multi-line report of this StackStats
    /// * filename - include source filename in stack trace
    pub fn rich_report(&self, with_filenames: bool) -> String {
        let profiled_alloc_bytes = YingProfiler::profiled_bytes_allocated();
        let pct = (self.allocated_bytes as f64) * 100.0 / (profiled_alloc_bytes as f64);
        let mut report = format!(
            "{} profiled bytes allocated ({pct:.2}%) ({} allocations)\n",
            self.allocated_bytes, self.num_allocations
        );
        let retained = self.retained_profiled_bytes();
        let _ = writeln!(
            &mut report,
            "  {} profiled bytes retained  ({} frees)",
            retained, self.num_frees
        );
        let retained_pct_allocs = (retained as f64) * 100.0 / (self.allocated_bytes as f64);
        let retained_pct_all =
            (retained as f64) * 100.0 / YingProfiler::profiled_bytes_retained() as f64;
        let _ = writeln!(
            &mut report,
            "    ({retained_pct_all:.2}% of all retained profiled allocs) ({retained_pct_allocs:.2}% of allocated bytes)",
        );

        #[cfg(feature = "profile-spans")]
        if !self.span.is_disabled() {
            let _ = writeln!(&mut report, "\ttracing span id: {:?}", self.span.id());
        }

        let decorated_stack = if with_filenames {
            self.stack.with_symbols_and_filename(&YING_STATE.symbol_map)
        } else {
            self.stack.with_symbols(&YING_STATE.symbol_map)
        };
        // We must lock out profiler as lots of allocations happen here, plus we access symbol map.
        // We must prevent profiler sampling as sampled allocations could change state and deadlock
        // due to trying to also access the symbol map.
        lock_out_profiler(|| {
            let _ = writeln!(&mut report, "{}", decorated_stack);
        });

        report
    }
}