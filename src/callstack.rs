use std::fmt;
use std::hash::Hasher;

use backtrace::BacktraceSymbol;
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
#[derive(Debug, Clone)]
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

    pub fn compute_hash(&self) -> u64 {
        let mut hasher = WyHash::with_seed(17);
        hasher.write(unsafe { (&self.frames).align_to::<u8>().1 });
        hasher.finish()
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

            // This is a concurrent hash map. It's OK for the contains/insert to not be atomic,
            // because for each IP the symbol should be identical, so multiple inserts are idempotent.
            if !symbol_map.contains_key(ip) {
                // IP not there. Get the corresponding frame from the backtrace
                let frame = &bt.frames()[i + TOP_FRAMES_TO_SKIP];

                // Get the symbol out.  Resolve the backtrace if necessary
                if frame.symbols().is_empty() {
                    bt.resolve();
                }
                let frame = &bt.frames()[i + TOP_FRAMES_TO_SKIP];

                // Clone backtrace and add it to symbol map
                symbol_map.insert(*ip, frame.clone());
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
            if let Some(frame) = self.symbols.get(ip) {
                writeln!(
                    f,
                    "  {}",
                    stringify_symbol(&frame.symbols()[0], self.filename_info)
                )?;
                if self.filter_poll {
                    for s in filtered_symbols_iter(&frame.symbols()[1..]) {
                        writeln!(f, "    > {}", stringify_symbol(s, self.filename_info))?;
                    }
                } else {
                    for s in &frame.symbols()[1..] {
                        writeln!(f, "    > {}", stringify_symbol(s, self.filename_info))?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn stringify_symbol(s: &BacktraceSymbol, include_filename: bool) -> String {
    if include_filename {
        format!(
            "{}\n\t({:?}:{})",
            s.name().expect("No symbol!"),
            s.filename().unwrap_or_else(|| std::path::Path::new("")),
            s.lineno().unwrap_or(0)
        )
    } else {
        format!("{}", s.name().expect("No symbol!"))
    }
}

/// Filter the symbols in a BacktraceFrame, returning an iterator.
/// Right now just filters out poll() calls.
fn filtered_symbols_iter(symbols: &[BacktraceSymbol]) -> impl Iterator<Item = &BacktraceSymbol> {
    symbols.iter().filter(|&s| {
        if let Some(symbol_name) = s.name() {
            let demangled = format!("{:?}", symbol_name);
            !demangled.contains("Future>::poll::")
        } else {
            false
        }
    })
}
