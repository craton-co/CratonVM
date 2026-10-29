// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot 25's code cache layout as the JMX non-heap pools show it (zt t30/nh,
//! `g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md` item 2).
//!
//! HotSpot builds one `MemoryPoolMXBean` per code heap
//! (`MemoryService::add_code_heap_memory_pool`, from `CodeCache::add_heap`),
//! with `init` = the heap's capacity right after it committed its first
//! `align_up(MIN2(InitialCodeCacheSize, heap reserved size), page)` and `max`
//! = the heap's reserved size. The heaps are:
//!
//! * segmented (`SegmentedCodeCache`; ergonomically on when tiered
//!   compilation is on and `ReservedCodeCacheSize >= 240M`): `CodeHeap
//!   'non-nmethods'` (`NonNMethodCodeHeapSize` 5M plus the compiler threads'
//!   code buffers, `CodeCache::initialize_heaps`), `CodeHeap 'profiled
//!   nmethods'` and `CodeHeap 'non-profiled nmethods'` (the rest, halved);
//! * unsegmented: ONE heap, `CodeCache`, of `ReservedCodeCacheSize`
//!   (240M with tiered compilation, else 48M).
//!
//! Pure; the VM resolves the arguments from its `-XX:` options.

/// HotSpot's pool name of the unsegmented code cache.
pub const UNSEGMENTED_CODE_CACHE_POOL: &str = "CodeCache";

const K: u64 = 1024;
const M: u64 = 1024 * K;

/// x86_64 `InitialCodeCacheSize` (c2_globals_x86.hpp: 2496K).
pub const DEFAULT_INITIAL_CODE_CACHE_SIZE: u64 = 2496 * K;
/// x86_64 `NonNMethodCodeHeapSize` before the compiler buffers (5M).
const DEFAULT_NON_NMETHOD_CODE_HEAP_SIZE: u64 = 5 * M;
/// `CODE_CACHE_DEFAULT_LIMIT` / the tiered default `ReservedCodeCacheSize`.
const TIERED_RESERVED_CODE_CACHE_SIZE: u64 = 240 * M;
/// x86_64 `ReservedCodeCacheSize` without tiered compilation (48M).
const PLAIN_RESERVED_CODE_CACHE_SIZE: u64 = 48 * M;
/// `os::vm_page_size()`: the commit granule of a code heap's first commit.
const PAGE: u64 = 4 * K;
/// `MAX2(os::vm_allocation_granularity(), page)`: the granule heap sizes are
/// aligned to (`initialize_heaps`' `min_size`); 64K on Windows.
const HEAP_GRANULE: u64 = if cfg!(windows) { 64 * K } else { 4 * K };
/// C1's per-thread code buffer (`Compiler::code_buffer_size()`:
/// `NMethodSizeLimit` (64K words) plus a tenth of it for constants).
const C1_CODE_BUFFER: u64 = 512 * K + 512 * K / 10;
/// C2's per-thread scratch buffer (`C2Compiler::initial_code_buffer_size()`:
/// `MAX_inst_size` 2048 + `MAX_stubs_size` 128 + the 4K constant table +
/// 2 x `end_slop` (32) + 128 two-byte `relocInfo`s).
const C2_CODE_BUFFER: u64 = 2048 + 128 + 4 * K + 64 + 256;

/// What the command line says about the code cache. `None` = not given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCacheArgs {
    pub reserved: Option<u64>,
    pub initial: Option<u64>,
    pub segmented: Option<bool>,
    pub non_nmethod: Option<u64>,
    /// `CompilerConfig::is_tiered()`: `TieredCompilation` on, not `-Xint`,
    /// `TieredStopAtLevel` 4.
    pub tiered: bool,
    /// `os::active_processor_count()`.
    pub cpus: u32,
    /// `-XX:CICompilerCount`.
    pub compiler_count: Option<u32>,
}

/// The JMX-visible shape of the code cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeCacheLayout {
    /// Three `CodeHeap '...'` pools (else one `CodeCache` pool).
    pub segmented: bool,
    /// Each pool's `init`, in HotSpot's pool order (`non-nmethods`,
    /// `profiled nmethods`, `non-profiled nmethods`); unsegmented: `[0]` is
    /// the `CodeCache` pool's, the others unused (0).
    pub inits: [u64; 3],
    /// Unsegmented: the `CodeCache` pool's `max` (the reserved size).
    pub reserved: u64,
}

impl CodeCacheLayout {
    /// HotSpot's default layout (x86_64, 4 CPUs or fewer differ only in the
    /// non-nmethods heap's size, which the 2496K first commit never reaches):
    /// three heaps of `init` 2555904.
    pub const DEFAULT_SEGMENTED: Self = Self {
        segmented: true,
        inits: [DEFAULT_INITIAL_CODE_CACHE_SIZE; 3],
        reserved: TIERED_RESERVED_CODE_CACHE_SIZE,
    };

    /// The `init` of code pool `name` (a `CodeHeap '...'` name or
    /// [`UNSEGMENTED_CODE_CACHE_POOL`]) in this layout; `None` for a pool
    /// this layout does not list.
    pub fn init_of(&self, name: &str) -> Option<u64> {
        match (self.segmented, name) {
            (true, "CodeHeap 'non-nmethods'") => Some(self.inits[0]),
            (true, "CodeHeap 'profiled nmethods'") => Some(self.inits[1]),
            (true, "CodeHeap 'non-profiled nmethods'") => Some(self.inits[2]),
            (false, UNSEGMENTED_CODE_CACHE_POOL) => Some(self.inits[0]),
            _ => None,
        }
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a).saturating_mul(a)
}

/// `log2i`: floor log2, 0 for 0.
fn log2i(v: u64) -> u64 {
    if v == 0 {
        0
    } else {
        63 - u64::from(v.leading_zeros())
    }
}

/// `(c1_count, c2_count)` as `CompilationPolicy::initialize` sets them under
/// `CICompilerCountPerCPU` (count = `MAX2(log2(n) * log2(MAX2(log2(n), 1)) * 3 / 2, 2)`),
/// or from `CICompilerCount`; c1 a third, at least one each.
fn compiler_counts(cpus: u32, explicit: Option<u32>) -> (u64, u64) {
    let count = match explicit {
        Some(c) => u64::from(c.max(2)),
        None => {
            let log_cpu = log2i(u64::from(cpus.max(1)));
            let loglog_cpu = log2i(log_cpu.max(1));
            (log_cpu * loglog_cpu * 3 / 2).max(2)
        }
    };
    let c1 = (count / 3).max(1);
    let c2 = count.saturating_sub(c1).max(1);
    (c1, c2)
}

/// HotSpot 25's layout for `args` (`CompilerConfig::ergo_initialize` for the
/// defaults, `CodeCache::initialize_heaps` / `add_heap` for the sizes). Pure.
pub fn hotspot_code_cache_layout(args: &CodeCacheArgs) -> CodeCacheLayout {
    let initial = args.initial.unwrap_or(DEFAULT_INITIAL_CODE_CACHE_SIZE);
    let reserved = args.reserved.unwrap_or(if args.tiered {
        TIERED_RESERVED_CODE_CACHE_SIZE
    } else {
        PLAIN_RESERVED_CODE_CACHE_SIZE
    });
    let reserved = align_up(reserved, HEAP_GRANULE);
    let segmented = args.segmented.unwrap_or(
        args.tiered && reserved >= TIERED_RESERVED_CODE_CACHE_SIZE && 8 * PAGE <= reserved,
    );
    let first_commit = |heap: u64| align_up(initial.min(heap), PAGE);
    if !segmented {
        return CodeCacheLayout {
            segmented,
            inits: [first_commit(reserved), 0, 0],
            reserved,
        };
    }
    let non_nmethod = match args.non_nmethod {
        Some(size) => size,
        None => {
            let (c1, c2) = compiler_counts(args.cpus, args.compiler_count);
            DEFAULT_NON_NMETHOD_CODE_HEAP_SIZE + c1 * C1_CODE_BUFFER + c2 * C2_CODE_BUFFER
        }
    };
    let non_nmethod = align_up(non_nmethod, HEAP_GRANULE);
    let nmethods = if reserved > non_nmethod + 2 * HEAP_GRANULE {
        (reserved - non_nmethod) / 2
    } else {
        HEAP_GRANULE
    };
    let nmethods = align_up(nmethods, HEAP_GRANULE);
    CodeCacheLayout {
        segmented,
        inits: [
            first_commit(non_nmethod),
            first_commit(nmethods),
            first_commit(nmethods),
        ],
        reserved,
    }
}

/// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 4): the
/// bytes of explicit huge pages (`large_page`, 2 MiB) HotSpot 25's code cache
/// takes from the hugetlbfs pool at start-up under `-XX:+UseLargePages`:
/// `CodeCache::initialize_heaps` picks the largest page size the reserved
/// size holds at least eight times (`page_size(false, 8)`), aligns each code
/// heap up to it and reserves their sum as one committed (special) mapping;
/// when the pool cannot give it the code cache silently uses small pages.
/// Measured on 25.0.4, 4 CPUs (`-Xlog:pagesize*=debug`, rows `zki_hs2`):
/// 242M (6M + 118M + 118M) by default, 240M with `-XX:-SegmentedCodeCache`,
/// 48M with `-Xint`, `-XX:-TieredCompilation` or
/// `-XX:ReservedCodeCacheSize=48m`. `None` when the code cache is below eight
/// large pages (it uses small pages then). Pure.
pub fn hotspot_code_cache_large_page_bytes(args: &CodeCacheArgs, large_page: u64) -> Option<u64> {
    let reserved = args.reserved.unwrap_or(if args.tiered {
        TIERED_RESERVED_CODE_CACHE_SIZE
    } else {
        PLAIN_RESERVED_CODE_CACHE_SIZE
    });
    if large_page == 0 || reserved < large_page.saturating_mul(8) {
        return None;
    }
    let reserved = align_up(reserved, HEAP_GRANULE);
    let segmented = args.segmented.unwrap_or(
        args.tiered && reserved >= TIERED_RESERVED_CODE_CACHE_SIZE && 8 * PAGE <= reserved,
    );
    if !segmented {
        return Some(align_up(reserved, large_page));
    }
    let non_nmethod = match args.non_nmethod {
        Some(size) => size,
        None => {
            let (c1, c2) = compiler_counts(args.cpus, args.compiler_count);
            DEFAULT_NON_NMETHOD_CODE_HEAP_SIZE + c1 * C1_CODE_BUFFER + c2 * C2_CODE_BUFFER
        }
    };
    let non_nmethod = align_up(non_nmethod, HEAP_GRANULE);
    let nmethods = if reserved > non_nmethod + 2 * HEAP_GRANULE {
        (reserved - non_nmethod) / 2
    } else {
        HEAP_GRANULE
    };
    let nmethods = align_up(nmethods, HEAP_GRANULE);
    Some(align_up(non_nmethod, large_page) + 2 * align_up(nmethods, large_page))
}

/// A HotSpot size option's value (`-XX:<Name>=<size>`: decimal or `0x` hex,
/// with an optional `k`/`m`/`g`/`t` suffix in either case). `None` when it
/// does not parse. Pure.
pub fn parse_hotspot_size(text: &str) -> Option<u64> {
    let (digits, shift) = match text.as_bytes().last()? {
        b'k' | b'K' => (&text[..text.len() - 1], 10),
        b'm' | b'M' => (&text[..text.len() - 1], 20),
        b'g' | b'G' => (&text[..text.len() - 1], 30),
        b't' | b'T' => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    let value = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u64>().ok()?,
    };
    value.checked_mul(1u64 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiered(cpus: u32) -> CodeCacheArgs {
        CodeCacheArgs {
            tiered: true,
            cpus,
            ..CodeCacheArgs::default()
        }
    }

    /// zki: the code cache's explicit huge pages on the host (25.0.4, 4 CPUs,
    /// `-Xlog:pagesize*=debug`: `size: 242M` / `240M` / `48M`).
    #[test]
    fn zki_code_cache_large_page_bytes_match_the_host() {
        let lp = 2 * M;
        assert_eq!(
            hotspot_code_cache_large_page_bytes(&tiered(4), lp),
            Some(242 * M)
        );
        let unsegmented = CodeCacheArgs {
            segmented: Some(false),
            ..tiered(4)
        };
        assert_eq!(
            hotspot_code_cache_large_page_bytes(&unsegmented, lp),
            Some(240 * M)
        );
        let plain = CodeCacheArgs {
            cpus: 4,
            ..CodeCacheArgs::default()
        };
        assert_eq!(
            hotspot_code_cache_large_page_bytes(&plain, lp),
            Some(48 * M)
        );
        let small = CodeCacheArgs {
            reserved: Some(48 * M),
            ..tiered(4)
        };
        assert_eq!(
            hotspot_code_cache_large_page_bytes(&small, lp),
            Some(48 * M)
        );
        // Below eight large pages the code cache keeps small pages.
        let tiny = CodeCacheArgs {
            reserved: Some(12 * M),
            ..tiered(4)
        };
        assert_eq!(hotspot_code_cache_large_page_bytes(&tiny, lp), None);
        let sixteen = CodeCacheArgs {
            reserved: Some(16 * M),
            ..tiered(4)
        };
        assert_eq!(
            hotspot_code_cache_large_page_bytes(&sixteen, lp),
            Some(16 * M)
        );
    }

    /// Host, HotSpot 25.0.4, 4 CPUs (and `ActiveProcessorCount=1`): three
    /// heaps of 2555904, sum 7667712; `ReservedCodeCacheSize=300m` the same.
    #[test]
    fn zt30nh_the_default_layout_is_three_heaps_of_2555904() {
        for cpus in [1, 4, 64] {
            let l = hotspot_code_cache_layout(&tiered(cpus));
            assert_eq!(
                l,
                CodeCacheLayout {
                    reserved: l.reserved,
                    ..CodeCacheLayout::DEFAULT_SEGMENTED
                }
            );
            assert_eq!(l.inits.iter().sum::<u64>(), 7_667_712);
        }
        let big = CodeCacheArgs {
            reserved: parse_hotspot_size("300m"),
            ..tiered(4)
        };
        assert_eq!(hotspot_code_cache_layout(&big).inits, [2_555_904; 3]);
    }

    /// Host: `ReservedCodeCacheSize=64m` (below 240M) or
    /// `-XX:-SegmentedCodeCache`: one `CodeCache` pool of init 2555904.
    #[test]
    fn zt30nh_below_240m_or_unsegmented_is_one_code_cache() {
        let small = CodeCacheArgs {
            reserved: parse_hotspot_size("64m"),
            ..tiered(4)
        };
        let l = hotspot_code_cache_layout(&small);
        assert!(!l.segmented);
        assert_eq!(l.init_of(UNSEGMENTED_CODE_CACHE_POOL), Some(2_555_904));
        assert_eq!(l.reserved, 64 << 20);
        assert_eq!(l.init_of("CodeHeap 'non-nmethods'"), None);
        let off = CodeCacheArgs {
            segmented: Some(false),
            ..tiered(4)
        };
        let l = hotspot_code_cache_layout(&off);
        assert!(!l.segmented);
        assert_eq!(l.reserved, 240 << 20);
        let plain = CodeCacheArgs {
            tiered: false,
            ..tiered(4)
        };
        assert_eq!(hotspot_code_cache_layout(&plain).reserved, 48 << 20);
        assert!(!hotspot_code_cache_layout(&plain).segmented);
    }

    /// Host, 4 CPUs: `InitialCodeCacheSize=8m` -> non-nmethods 5836800 (its
    /// whole size), profiled and non-profiled 8388608; sum 22614016.
    #[cfg(not(windows))]
    #[test]
    fn zt30nh_initial_code_cache_size_moves_the_inits() {
        let a = CodeCacheArgs {
            initial: parse_hotspot_size("8m"),
            ..tiered(4)
        };
        let l = hotspot_code_cache_layout(&a);
        assert_eq!(l.inits, [5_836_800, 8_388_608, 8_388_608]);
        assert_eq!(l.inits.iter().sum::<u64>(), 22_614_016);
    }

    #[test]
    fn zt30nh_compiler_counts_follow_cicompilercountpercpu() {
        assert_eq!(compiler_counts(1, None), (1, 1));
        assert_eq!(compiler_counts(2, None), (1, 1));
        assert_eq!(compiler_counts(4, None), (1, 2));
        assert_eq!(compiler_counts(8, None), (1, 3));
        assert_eq!(compiler_counts(16, None), (4, 8));
        assert_eq!(compiler_counts(4, Some(6)), (2, 4));
    }

    #[test]
    fn zt30nh_hotspot_sizes_parse() {
        assert_eq!(parse_hotspot_size("64m"), Some(64 << 20));
        assert_eq!(parse_hotspot_size("2496K"), Some(2_555_904));
        assert_eq!(parse_hotspot_size("0x1000"), Some(4096));
        assert_eq!(parse_hotspot_size("1g"), Some(1 << 30));
        assert_eq!(parse_hotspot_size("123"), Some(123));
        assert_eq!(parse_hotspot_size("x"), None);
        assert_eq!(parse_hotspot_size(""), None);
    }
}
