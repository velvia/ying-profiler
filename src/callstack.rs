use std::borrow::Cow;
use std::fmt;
use std::hash::Hasher;

use backtrace::BacktraceSymbol;
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
        hasher.write(unsafe { (self.frames).align_to::<u8>().1 });
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

                // Convert frame symbols into FriendlySymbols and add to symbol map
                let friendlies = frame.symbols().iter().map(FriendlySymbol::from).collect();
                symbol_map.insert(*ip, friendlies);
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
            if let Some(symbols) = self.symbols.get(ip) {
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
        let friendly_name = if let Some(symbolname) = s.name() {
            let demangled = format!("{}", symbolname);
            SYMBOL_REGEXES
                .name_end_re
                .replace(&demangled, "")
                .into_owned()
        } else {
            "<none>".into()
        };

        let is_poll = friendly_name.contains("::poll::");

        // Get filename and convert common patterns
        let shorter_filename = if let Some(p) = s.filename() {
            let filename = p.to_str().unwrap_or_default();
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
