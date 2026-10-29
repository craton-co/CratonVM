// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot young-generation sizing (`-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`,
//! `-XX:NewRatio`) for the generational backend.
//!
//! gen r4w2/alloc2 (2026-09-23). Until this module the four flags were
//! swallowed by the launcher's `-X…` / `-XX:…` catch-alls and the young
//! generation was hard-wired by [`crate::gen_heap::GenerationalHeap::with_capacity`]
//! to half of `-Xmx` (one quarter per semi-space) —
//! `docs/feature-designs/generative-gc-architecture.md`.
//!
//! # Semantics
//!
//! HotSpot's young generation is eden plus two survivor spaces; this
//! collector's is the from/to semi-space PAIR. A young size therefore maps to
//! the pair, and each semi-space gets half of it.
//!
//! * `-Xmn<size>` sets the young size exactly (HotSpot: `NewSize = MaxNewSize
//!   = Xmn`). It wins over the other three, as in HotSpot.
//! * Otherwise the base is `-Xmx / (NewRatio + 1)` when `-XX:NewRatio` is
//!   given, else this collector's own default (`-Xmx / 2`).
//! * `-XX:NewSize` is a FLOOR and `-XX:MaxNewSize` a CEILING on that base.
//!   HotSpot starts the young generation at `NewSize` and lets it grow toward
//!   `MaxNewSize`; this collector's young pair does not resize once built (the
//!   `-Xmx` budget leaves the growth path nothing to give — see
//!   `with_capacity`), so the honest single-number reading of the pair is
//!   "the base, held between the two". A `NewSize` above `MaxNewSize` raises
//!   the ceiling to `NewSize`, which is HotSpot's resolution too.
//! * The old generation must stay non-empty: the pair is clamped so at least
//!   [`MIN_OLD_GEN_BYTES`] (or half the heap, on a heap smaller than twice
//!   that) is left for it, and each semi-space is at least [`YOUNG_SEMI_MIN`].
//!   Every clamp or override is reported in [`YoungGenPlan::adjustments`], which
//!   the launcher prints once at startup.
//!
//! With none of the four given, [`plan_young_gen`] answers `None` and the heap
//! is built by `with_capacity`, byte for byte as before.
//!
//! G1 and ZGC do not consult this: G1 sizes its young generation adaptively
//! from region counts, ZGC has no young generation. The launcher tells the
//! operator so, once.

/// The smallest young semi-space this module will plan: the floor
/// `GenerationalHeap::with_sizes_and_max_young` applies to every arena
/// (`young_semi_size.max(1024)`), so a planned pair is never silently widened
/// past the `-Xmx` budget by the constructor.
pub const YOUNG_SEMI_MIN: usize = 1024;

/// The old generation keeps at least this much of the heap when an explicit
/// young size would otherwise take it all (or half the heap, when the heap is
/// smaller than twice this). HotSpot's equivalent floor is one generation
/// alignment; one MiB is of that order and keeps a tenure from failing on the
/// very first promoted object.
pub const MIN_OLD_GEN_BYTES: usize = 1 << 20;

/// The smallest total heap the generational constructors build; mirrors
/// `with_capacity`'s `total_bytes.max(4096)`.
const MIN_TOTAL_BYTES: usize = 4096;

/// What the operator asked for, flag by flag. `None` = not given.
///
/// Per VM (it travels on `VmConfig`), never a process global.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct YoungGenSizing {
    /// `-Xmn<size>`, bytes.
    pub xmn: Option<usize>,
    /// `-XX:NewSize=<size>`, bytes.
    pub new_size: Option<usize>,
    /// `-XX:MaxNewSize=<size>`, bytes.
    pub max_new_size: Option<usize>,
    /// `-XX:NewRatio=<n>`: old / young.
    pub new_ratio: Option<usize>,
}

impl YoungGenSizing {
    /// `true` when none of the four flags was given — the default sizing.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The HotSpot spellings of the flags that were given, in a fixed order —
    /// for the launcher's "ignored on this collector" note.
    pub fn given_flag_names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.xmn.is_some() {
            out.push("-Xmn");
        }
        if self.new_size.is_some() {
            out.push("-XX:NewSize");
        }
        if self.max_new_size.is_some() {
            out.push("-XX:MaxNewSize");
        }
        if self.new_ratio.is_some() {
            out.push("-XX:NewRatio");
        }
        out
    }
}

/// The resolved geometry for an explicit young size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoungGenPlan {
    /// The heap budget the plan divides (`-Xmx`, floored like `with_capacity`).
    pub total_bytes: usize,
    /// Each young semi-space, bytes (8-aligned).
    pub young_semi: usize,
    /// The old generation, bytes: `total_bytes - 2 * young_semi`.
    pub old_size: usize,
    /// The young pair the flags asked for, before any clamp.
    pub requested_pair: usize,
    /// Which flag decided the size (`"-Xmn"`, `"-XX:NewRatio"`, ...).
    pub source: &'static str,
    /// One human-readable line per clamp or override. Empty when the request
    /// was honoured exactly.
    pub adjustments: Vec<String>,
}

impl YoungGenPlan {
    /// The young pair actually planned (`2 * young_semi`).
    pub fn young_pair(&self) -> usize {
        self.young_semi.saturating_mul(2)
    }

    /// One line for `--verbose:gc`.
    pub fn summary(&self) -> String {
        format!(
            "[cratonvm] generational young generation: {} ({}; 2 x {} semi-spaces), \
             old generation {}, heap {}",
            fmt_bytes(self.young_pair()),
            self.source,
            fmt_bytes(self.young_semi),
            fmt_bytes(self.old_size),
            fmt_bytes(self.total_bytes),
        )
    }
}

/// Render a byte count the way the flags are usually written.
fn fmt_bytes(n: usize) -> String {
    const K: usize = 1024;
    if n >= K * K && n.is_multiple_of(K * K) {
        format!("{} MiB", n / (K * K))
    } else if n >= K && n.is_multiple_of(K) {
        format!("{} KiB", n / K)
    } else {
        format!("{n} bytes")
    }
}

/// This collector's default young semi-space for a `total_bytes` heap — the
/// arithmetic `GenerationalHeap::with_capacity` uses.
pub fn default_young_semi(total_bytes: usize) -> usize {
    (total_bytes.max(MIN_TOTAL_BYTES) / 4).max(YOUNG_SEMI_MIN)
}

/// Resolve `req` against a `total_bytes` heap. `None` when no flag was given:
/// the caller must then build the default heap, unchanged.
pub fn plan_young_gen(total_bytes: usize, req: &YoungGenSizing) -> Option<YoungGenPlan> {
    // An explicit zero `-XX:NewSize` / `-XX:MaxNewSize` is HotSpot's "the
    // ergonomic minimum" ([`hotspot_serial_young_ergonomics`] answers its
    // lines), not a geometry this collector plans; `-Xmn0` is refused by the
    // launcher.
    let req = &YoungGenSizing {
        new_size: req.new_size.filter(|&n| n != 0),
        max_new_size: req.max_new_size.filter(|&n| n != 0),
        ..*req
    };
    if req.is_default() {
        return None;
    }
    let total = total_bytes.max(MIN_TOTAL_BYTES);
    let mut adjustments = Vec::new();

    let (pair, source) = if let Some(xmn) = req.xmn {
        if req.new_size.is_some() || req.max_new_size.is_some() {
            adjustments.push(
                "-Xmn sets the young size exactly; -XX:NewSize / -XX:MaxNewSize are ignored"
                    .to_string(),
            );
        }
        if req.new_ratio.is_some() {
            adjustments
                .push("-Xmn sets the young size exactly; -XX:NewRatio is ignored".to_string());
        }
        (xmn, "-Xmn")
    } else {
        let (base, base_source) = match req.new_ratio {
            Some(r) => (total / r.saturating_add(1), "-XX:NewRatio"),
            None => (
                default_young_semi(total).saturating_mul(2),
                "default -Xmx/2",
            ),
        };
        let floor = req.new_size;
        let mut ceiling = req.max_new_size;
        if let (Some(n), Some(m)) = (floor, ceiling) {
            if n > m {
                adjustments.push(format!(
                    "-XX:NewSize ({}) is larger than -XX:MaxNewSize ({}); a maximum young \
                     size of {} will be used",
                    fmt_bytes(n),
                    fmt_bytes(m),
                    fmt_bytes(n),
                ));
                ceiling = Some(n);
            }
        }
        let mut pair = base;
        let mut source = base_source;
        if let Some(m) = ceiling {
            if pair > m {
                pair = m;
                source = "-XX:MaxNewSize";
            }
        }
        if let Some(n) = floor {
            if pair < n {
                pair = n;
                source = "-XX:NewSize";
            }
        }
        (pair, source)
    };

    // Clamp: the old generation stays non-empty, and each semi is usable.
    let min_old = MIN_OLD_GEN_BYTES.min(total / 2);
    let max_semi = ((total - min_old) / 2) & !7;
    let mut young_semi = (pair / 2) & !7;
    if young_semi > max_semi {
        adjustments.push(format!(
            "{source} asks for a {} young generation in a {} heap; clamped to {} so the \
             old generation keeps {}",
            fmt_bytes(pair),
            fmt_bytes(total),
            fmt_bytes(max_semi * 2),
            fmt_bytes(total - max_semi * 2),
        ));
        young_semi = max_semi;
    }
    if young_semi < YOUNG_SEMI_MIN {
        adjustments.push(format!(
            "{source} asks for a {} young generation; raised to the minimum of {}",
            fmt_bytes(pair),
            fmt_bytes(YOUNG_SEMI_MIN * 2),
        ));
        young_semi = YOUNG_SEMI_MIN;
    }
    let old_size = total - young_semi * 2;
    Some(YoungGenPlan {
        total_bytes: total,
        young_semi,
        old_size,
        requested_pair: pair,
        source,
        adjustments,
    })
}

/// gcj j3/yg3 -- the HotSpot Serial young geometry INSIDE this collector's
/// semi-space pair (`gcj-j1yg-proposal-serial-young-geometry-and-no-pause-goal`
/// part 1). Armed by `CRATONVM_GEN_SERIAL_YOUNG_GEOMETRY` (gcj yg3-residuals R3,
/// `GenerationalHeap::with_serial_young_geometry`), which reserves `Y` per semi
/// (one survivor more, the pinned in-place copy's survivor space) and reports
/// `semi_reserve` to every Java-visible reader.
///
/// HotSpot's young generation `Y` is eden plus two survivors
/// (`DefNewGeneration::compute_space_boundaries`: `survivor =
/// align_down(Y / (SurvivorRatio + 2), SpaceAlignment)`, at least one
/// alignment; `eden = Y - 2 x survivor`). This collector's pair swaps roles,
/// so it cannot give one semi eden and the other a survivor. It does not need
/// to: a young cycle copies at most a SURVIVOR's worth into to-space and
/// tenures the rest (the serial Cheney arm's overflow promotion,
/// `CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`, and the parallel arm's), so after
/// the swap the new from-space starts with at most `survivor` bytes and the
/// mutator fills `eden` more on top of them. Each semi then needs `eden +
/// survivor` of RESERVATION, and the young COMMIT never exceeds
/// `eden + 2 x survivor = Y` (the from-space's `eden + survivor` plus the
/// to-space's `survivor` copy cap), so `-Xmx = Y + old` holds for every reader
/// that sums commits, with the old generation exactly HotSpot's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialPairGeometry {
    /// The young budget `Y` (the plan's pair), on the alignment grid.
    pub young_budget: usize,
    /// HotSpot's eden: the young trigger.
    pub eden: usize,
    /// One HotSpot survivor: the to-space copy cap.
    pub survivor: usize,
    /// Each semi as a moving cycle uses it and as the readers report it,
    /// `eden + survivor` (the heap reserves `young_budget`; see above).
    pub semi_reserve: usize,
}

impl SerialPairGeometry {
    /// The most young bytes committed at any point of a cycle: the
    /// from-space's `semi_reserve` plus the to-space's copy cap, `== Y`.
    pub fn max_young_commit(&self) -> usize {
        self.semi_reserve + self.survivor
    }
}

/// gcj j3/yg3 -- [`SerialPairGeometry`] for a young budget `young_pair` (an
/// explicit plan's [`YoungGenPlan::young_pair`]), `-XX:SurvivorRatio`
/// `survivor_ratio` (at least 1) and `alignment` (HotSpot's `SpaceAlignment`,
/// a power of two). `None` when the budget cannot hold one aligned eden and
/// two aligned survivors. Pure.
pub fn plan_serial_pair_geometry(
    young_pair: usize,
    survivor_ratio: usize,
    alignment: usize,
) -> Option<SerialPairGeometry> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    let y = young_pair & !(alignment - 1);
    let per = y / survivor_ratio.max(1).saturating_add(2);
    let survivor = (per & !(alignment - 1)).max(alignment);
    let eden = y.checked_sub(survivor.checked_mul(2)?)?;
    if eden < alignment {
        return None;
    }
    Some(SerialPairGeometry {
        young_budget: y,
        eden,
        survivor,
        semi_reserve: eden + survivor,
    })
}

/// HotSpot 25's default `NewSize` on a 64-bit VM: `ScaleForWordSize(1 * M)`
/// (`1 M * 13 / 10`, aligned down to a heap word), the `1331k` of G1's and
/// Serial's `NewSize (1331k) is greater than the MaxNewSize (0k)` warning.
pub const HOTSPOT_DEFAULT_NEW_SIZE: usize = (1024 * 1024 * 13 / 10) & !7;

/// HotSpot 25 Serial's `SpaceAlignment` and `GenAlignment`
/// (`Generation::GenGrain`, 64 KiB).
pub const HOTSPOT_SERIAL_GEN_ALIGNMENT: usize = 64 * 1024;

/// The command-line state HotSpot 25 Serial's young-size ergonomics read:
/// the heap flags after `Arguments::set_heap_size` (this launcher's resolved
/// `-Xmx` / initial / minimum heap, aligned to the collector's
/// `HeapAlignment`) and the young flags as typed. Per VM, never a global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialYoungFlags {
    pub max_heap: usize,
    pub initial_heap: usize,
    pub min_heap: usize,
    /// `-XX:NewSize` (or `-Xmn`), typed, zero included.
    pub new_size: Option<usize>,
    /// `-XX:MaxNewSize` (or `-Xmn`), typed, zero included.
    pub max_new_size: Option<usize>,
    /// `-XX:NewRatio`, default 2.
    pub new_ratio: usize,
    /// `-XX:SurvivorRatio`, default 8.
    pub survivor_ratio: usize,
}

impl SerialYoungFlags {
    /// The flags for `req` over the heap sizes (`-Xmn` sets both sizes, as
    /// HotSpot's `parse_each_vm_init_arg` does; it wins over a typed
    /// `NewSize` / `MaxNewSize` here, as in `g1_young_sizer_from_flags`).
    pub fn new(
        max_heap: usize,
        initial_heap: usize,
        min_heap: usize,
        req: &YoungGenSizing,
        survivor_ratio: Option<usize>,
    ) -> Self {
        Self {
            max_heap,
            initial_heap,
            min_heap,
            new_size: req.xmn.or(req.new_size),
            max_new_size: req.xmn.or(req.max_new_size),
            new_ratio: req.new_ratio.unwrap_or(2),
            survivor_ratio: survivor_ratio.unwrap_or(8),
        }
    }
}

/// What HotSpot 25 Serial's `GenArguments` decides for [`SerialYoungFlags`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialYoungErgonomics {
    /// The `log_warning(gc, ergo)` texts, in HotSpot's order.
    pub warnings: Vec<String>,
    /// The young generation HotSpot starts with (`NewSize` after
    /// `initialize_size_info`).
    pub initial_young: usize,
    /// Its eden: the young size less two survivors
    /// (`DefNewGeneration::compute_space_boundaries`).
    pub initial_eden: usize,
}

/// gc-known-issues retire (`launcher-hotspot-flag-table-residuals-FIXED-20261025`
/// item 2): HotSpot 25's `GenArguments::initialize_heap_flags_and_sizes` and
/// `initialize_size_info` (`gc/serial/genArguments.cpp`, jdk25u) over `f`,
/// with Serial's 64 KiB `SpaceAlignment` / `GenAlignment`: the
/// `[warning][gc,ergo]` texts it logs and the initial young geometry.
///
/// The flags' "set on the command line" bit survives `FLAG_SET_ERGO`
/// (`JVMFlag::set_origin`), so a typed size stays "command line" after an
/// ergonomic correction, as HotSpot reads it. Pure.
pub fn hotspot_serial_young_ergonomics(f: &SerialYoungFlags) -> SerialYoungErgonomics {
    const K: usize = 1024;
    let space = HOTSPOT_SERIAL_GEN_ALIGNMENT;
    let gen = HOTSPOT_SERIAL_GEN_ALIGNMENT;
    let align_down = |v: usize, a: usize| v & !(a - 1);
    let align_up = |v: usize, a: usize| v.saturating_add(a - 1) & !(a - 1);
    let bound_minus = |desired: usize, maximum: usize| desired.min(maximum.saturating_sub(gen));
    // `scale_by_NewRatio_aligned`: `align_down_bounded(base / (NewRatio + 1))`.
    let scale = |base: usize| {
        let v = align_down(base / f.new_ratio.saturating_add(1), gen);
        if v == 0 {
            gen
        } else {
            v
        }
    };
    let mut warnings = Vec::new();
    let new_cmdline = f.new_size.is_some();
    let max_cmdline = f.max_new_size.is_some();
    let max_heap = f.max_heap;
    let initial_heap = f.initial_heap.min(max_heap);
    let min_heap = f.min_heap.min(initial_heap);

    let smallest_new_size = align_up(3 * space, gen);
    let mut new_size = f.new_size.unwrap_or(HOTSPOT_DEFAULT_NEW_SIZE);
    let mut max_new_size = f.max_new_size.unwrap_or(usize::MAX);

    if new_cmdline && new_size >= initial_heap {
        let revised = bound_minus(new_size, initial_heap);
        warnings.push(format!(
            "NewSize ({}k) is equal to or greater than initial heap size ({}k).  A new \
             NewSize of {}k will be used to accomodate an old generation.",
            new_size / K,
            initial_heap / K,
            revised / K
        ));
        new_size = revised;
    }
    let bounded = smallest_new_size.max(align_down(bound_minus(new_size, max_heap), gen));
    new_size = bounded;
    let mut min_new_size = smallest_new_size;

    if max_cmdline {
        if max_new_size >= max_heap {
            let smaller = max_heap.saturating_sub(gen);
            warnings.push(format!(
                "MaxNewSize ({}k) is equal to or greater than the entire heap ({}k).  A new \
                 max generation size of {}k will be used.",
                max_new_size / K,
                max_heap / K,
                smaller / K
            ));
            max_new_size = smaller;
            new_size = new_size.min(max_new_size);
        } else if max_new_size < new_size {
            max_new_size = new_size;
        } else {
            max_new_size = align_down(max_new_size, gen);
        }
    }
    if new_size > max_new_size {
        if max_cmdline {
            warnings.push(format!(
                "NewSize ({}k) is greater than the MaxNewSize ({}k). A new max generation \
                 size of {}k will be used.",
                new_size / K,
                max_new_size / K,
                new_size / K
            ));
        }
        max_new_size = new_size;
    }
    let old_size = align_up(space, gen);
    if new_size + old_size > max_heap {
        // `-Xmx` is always on this launcher's command line or its ergonomic
        // default; HotSpot shrinks NewSize to fit an explicit one.
        let shrink = max_heap as f64 / (new_size + old_size) as f64;
        new_size = smallest_new_size.max(align_down((new_size as f64 * shrink) as usize, gen));
    }

    // `initialize_size_info`.
    let mut max_young = max_new_size;
    if !max_cmdline {
        max_young = scale(max_heap).clamp(new_size, max_new_size.max(new_size));
    }
    let mut initial_young = new_size;
    if max_heap == initial_heap {
        if new_cmdline {
            max_young = new_size;
        }
        initial_young = max_young;
        if max_heap == min_heap {
            min_new_size = max_young;
        }
    } else if new_cmdline {
        initial_young = max_young.min(bound_minus(new_size, initial_heap));
        min_new_size = bound_minus(initial_young, min_heap);
    } else {
        initial_young = scale(initial_heap).clamp(new_size, max_young.max(new_size));
        min_new_size = scale(min_heap).min(initial_young);
    }
    let max_old = max_heap.saturating_sub(max_young).max(gen);
    let min_old = max_old
        .min(initial_heap.saturating_sub(initial_young))
        .min(min_heap.saturating_sub(min_new_size));
    let mut initial_old = initial_heap
        .saturating_sub(initial_young)
        .clamp(min_old, max_old);
    if initial_old + initial_young != initial_heap {
        warnings.push(
            "Inconsistency between generation sizes and heap size, resizing the generations \
             to fit the heap."
                .to_string(),
        );
        let desired_young = initial_heap.saturating_sub(initial_old);
        if initial_heap < initial_old {
            initial_young = min_new_size;
            initial_old = initial_heap.saturating_sub(min_new_size);
        } else if desired_young > max_young {
            initial_young = max_young;
            initial_old = initial_heap.saturating_sub(max_young);
        } else if desired_young < min_new_size {
            initial_young = min_new_size;
            initial_old = initial_heap.saturating_sub(min_new_size);
        } else {
            initial_young = desired_young;
        }
    }
    let _ = initial_old;
    // `DefNewGeneration::compute_space_boundaries`.
    let survivor = align_down(
        initial_young / f.survivor_ratio.max(1).saturating_add(2),
        space,
    )
    .max(space);
    let initial_eden = initial_young.saturating_sub(2 * survivor);
    SerialYoungErgonomics {
        warnings,
        initial_young,
        initial_eden,
    }
}

/// The largest eden in which HotSpot 25 still fails its own bootstrap with
/// `GC triggered before VM initialization completed` (measured with
/// `-Xmx256m -Xms256m -XX:+UseSerialGC -XX:NewSize=<n>`, gc-known-issues
/// retire 2026-10-25): the JDK's boot allocations need more eden than this
/// before the VM counts as initialized. With the CDS archive on (`-Xshare`
/// other than `off`) Linux maps the archived heap and boots in the smallest
/// eden (64 KiB, Ubuntu 25.0.4.1), Windows loads it into the heap and fails
/// at 64 KiB (Microsoft 25.0.3); with `-Xshare:off` Linux fails up to
/// 128 KiB and Windows up to 192 KiB. `0`: never.
pub fn hotspot_serial_boot_failing_eden(sharing_off: bool) -> usize {
    const K: usize = 1024;
    match (cfg!(target_os = "windows"), sharing_off) {
        (false, false) => 0,
        (false, true) => 128 * K,
        (true, false) => 64 * K,
        (true, true) => 192 * K,
    }
}

/// HotSpot's `GC triggered before VM initialization completed. Try
/// increasing NewSize, current value <n><unit>.` for a young size, after
/// `Error occurred during initialization of VM` (`vm_exit_during_initialization`,
/// stdout, exit 1). `<n><unit>` is `byte_size_in_proper_unit`: bytes below
/// 100 KiB, K below 100 MiB, M below 100 GiB, then G. Pure.
pub fn hotspot_gc_before_init_lines(initial_young: usize) -> [String; 2] {
    const K: usize = 1024;
    let (n, unit) = if initial_young >= 100 * K * K * K {
        (initial_young / (K * K * K), "G")
    } else if initial_young >= 100 * K * K {
        (initial_young / (K * K), "M")
    } else if initial_young >= 100 * K {
        (initial_young / K, "K")
    } else {
        (initial_young, "B")
    };
    [
        "Error occurred during initialization of VM".to_string(),
        format!(
            "GC triggered before VM initialization completed. Try increasing NewSize, \
             current value {n}{unit}."
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: usize = 1 << 20;
    const G: usize = 1 << 30;

    const K: usize = 1 << 10;

    fn serial(max: usize, initial: usize, req: YoungGenSizing) -> SerialYoungErgonomics {
        hotspot_serial_young_ergonomics(&SerialYoungFlags::new(max, initial, 6 * M, &req, None))
    }

    /// gc-known-issues retire: HotSpot 25's Serial `[gc,ergo]` texts at
    /// `-Xmx256m` (Ubuntu 25.0.4.1 and Microsoft 25.0.3, 2026-10-25).
    #[test]
    fn serial_young_ergonomics_answer_as_hotspot() {
        let size = |n: Option<usize>, m: Option<usize>| YoungGenSizing {
            new_size: n,
            max_new_size: m,
            ..Default::default()
        };
        let t = 1usize << 40;
        let new_size_line = |n: usize, i: usize, r: usize| {
            format!(
                "NewSize ({n}k) is equal to or greater than initial heap size ({i}k).  A new \
                 NewSize of {r}k will be used to accomodate an old generation."
            )
        };
        let max_line = |n: usize| {
            format!(
                "MaxNewSize ({n}k) is equal to or greater than the entire heap (262144k).  A \
                 new max generation size of 262080k will be used."
            )
        };
        let inconsistency = "Inconsistency between generation sizes and heap size, resizing \
                             the generations to fit the heap.";
        assert_eq!(
            serial(256 * M, 256 * M, size(Some(t), None)).warnings,
            vec![new_size_line(1073741824, 262144, 262080)]
        );
        assert_eq!(
            serial(256 * M, 256 * M, size(None, Some(t))).warnings,
            vec![max_line(1073741824)]
        );
        let xmn = YoungGenSizing {
            xmn: Some(t),
            ..Default::default()
        };
        assert_eq!(
            serial(256 * M, 256 * M, xmn).warnings,
            vec![
                new_size_line(1073741824, 262144, 262080),
                max_line(1073741824)
            ]
        );
        assert_eq!(
            serial(256 * M, 16 * M, size(Some(32 * M), None)).warnings,
            vec![new_size_line(32768, 16384, 16320)]
        );
        assert_eq!(
            serial(256 * M, 16 * M, size(None, Some(300 * M))).warnings,
            vec![max_line(307200), inconsistency.to_string()]
        );
        assert!(serial(256 * M, 256 * M, size(Some(64 * M), Some(32 * M)))
            .warnings
            .is_empty());
        // `NewSize=0`: the smallest young generation, eden one alignment.
        let zero = serial(256 * M, 256 * M, size(Some(0), None));
        assert!(zero.warnings.is_empty());
        assert_eq!((zero.initial_young, zero.initial_eden), (192 * K, 64 * K));
        // Microsoft 25.0.3 on a 16 GiB host (initial 254M): HotSpot resizes the
        // young generation to 87359488 bytes.
        let windows = serial(256 * M, 266_338_304, size(Some(0), None));
        assert_eq!(windows.warnings, vec![inconsistency.to_string()]);
        assert_eq!(windows.initial_young, 87_359_488);
        // Defaults: no line.
        assert!(serial(256 * M, 256 * M, YoungGenSizing::default())
            .warnings
            .is_empty());
        assert_eq!(
            hotspot_gc_before_init_lines(192 * K)[1],
            "GC triggered before VM initialization completed. Try increasing NewSize, \
             current value 192K."
        );
        assert_eq!(HOTSPOT_DEFAULT_NEW_SIZE / K, 1331);
    }

    #[test]
    fn an_explicit_zero_size_plans_nothing() {
        let zero = YoungGenSizing {
            new_size: Some(0),
            max_new_size: Some(0),
            ..Default::default()
        };
        assert_eq!(plan_young_gen(G, &zero), None);
    }

    #[test]
    fn no_flag_means_no_plan() {
        assert!(YoungGenSizing::default().is_default());
        assert_eq!(plan_young_gen(G, &YoungGenSizing::default()), None);
    }

    #[test]
    fn xmn_is_the_young_pair() {
        let req = YoungGenSizing {
            xmn: Some(256 * M),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("a flag was given");
        assert_eq!(p.young_semi, 128 * M);
        assert_eq!(p.young_pair(), 256 * M);
        assert_eq!(p.old_size, G - 256 * M);
        assert_eq!(p.source, "-Xmn");
        assert!(p.adjustments.is_empty(), "{:?}", p.adjustments);
    }

    #[test]
    fn new_ratio_divides_the_heap() {
        // NewRatio=2: young = 1/3 of the heap.
        let req = YoungGenSizing {
            new_ratio: Some(2),
            ..Default::default()
        };
        let p = plan_young_gen(3 * G, &req).expect("plan");
        assert_eq!(p.young_pair(), G);
        assert_eq!(p.old_size, 2 * G);
        // NewRatio=1 is this collector's default split.
        let one = YoungGenSizing {
            new_ratio: Some(1),
            ..Default::default()
        };
        let p1 = plan_young_gen(G, &one).expect("plan");
        assert_eq!(p1.young_semi, default_young_semi(G));
    }

    #[test]
    fn xmn_wins_over_the_other_three_and_says_so() {
        let req = YoungGenSizing {
            xmn: Some(64 * M),
            new_size: Some(512 * M),
            max_new_size: Some(512 * M),
            new_ratio: Some(1),
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_pair(), 64 * M);
        assert_eq!(p.adjustments.len(), 2, "{:?}", p.adjustments);
    }

    #[test]
    fn new_size_is_a_floor_and_max_new_size_a_ceiling() {
        // Default base is 512 MiB of a 1 GiB heap.
        let cap = YoungGenSizing {
            max_new_size: Some(128 * M),
            ..Default::default()
        };
        assert_eq!(plan_young_gen(G, &cap).expect("plan").young_pair(), 128 * M);
        let above = YoungGenSizing {
            max_new_size: Some(900 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &above).expect("plan").young_pair(),
            512 * M
        );
        let floor = YoungGenSizing {
            new_size: Some(768 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &floor).expect("plan").young_pair(),
            768 * M
        );
        let below = YoungGenSizing {
            new_size: Some(64 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &below).expect("plan").young_pair(),
            512 * M
        );
        // Both, and with a ratio base.
        let both = YoungGenSizing {
            new_size: Some(100 * M),
            max_new_size: Some(200 * M),
            new_ratio: Some(3),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &both).expect("plan").young_pair(),
            200 * M
        );
    }

    #[test]
    fn a_new_size_above_max_new_size_raises_the_ceiling() {
        let req = YoungGenSizing {
            new_size: Some(300 * M),
            max_new_size: Some(100 * M),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_pair(), 300 * M);
        assert_eq!(p.adjustments.len(), 1, "{:?}", p.adjustments);
    }

    #[test]
    fn the_old_generation_is_never_emptied() {
        for req in [
            YoungGenSizing {
                xmn: Some(G),
                ..Default::default()
            },
            YoungGenSizing {
                xmn: Some(4 * G),
                ..Default::default()
            },
            YoungGenSizing {
                new_ratio: Some(0),
                ..Default::default()
            },
            YoungGenSizing {
                new_size: Some(usize::MAX),
                ..Default::default()
            },
        ] {
            let p = plan_young_gen(G, &req).expect("plan");
            assert!(p.old_size >= MIN_OLD_GEN_BYTES, "{req:?} -> {p:?}");
            assert_eq!(p.old_size + p.young_pair(), G, "{req:?}");
            assert_eq!(p.young_semi % 8, 0);
            assert_eq!(
                p.adjustments.len(),
                1,
                "exactly one clamp note: {:?}",
                p.adjustments
            );
        }
    }

    #[test]
    fn a_tiny_young_request_is_raised_to_the_minimum() {
        let req = YoungGenSizing {
            xmn: Some(8),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_semi, YOUNG_SEMI_MIN);
        assert_eq!(p.adjustments.len(), 1);
    }

    #[test]
    fn a_tiny_heap_still_plans_a_coherent_geometry() {
        for total in [0usize, 4096, 64 * 1024, 3 * M] {
            let req = YoungGenSizing {
                xmn: Some(usize::MAX),
                ..Default::default()
            };
            let p = plan_young_gen(total, &req).expect("plan");
            assert!(p.young_semi >= YOUNG_SEMI_MIN);
            assert!(p.old_size > 0);
            assert_eq!(p.old_size + p.young_pair(), total.max(MIN_TOTAL_BYTES));
        }
    }

    #[test]
    fn the_flag_names_follow_what_was_given() {
        let req = YoungGenSizing {
            xmn: Some(1),
            new_ratio: Some(2),
            ..Default::default()
        };
        assert_eq!(req.given_flag_names(), vec!["-Xmn", "-XX:NewRatio"]);
        assert!(YoungGenSizing::default().given_flag_names().is_empty());
    }

    #[test]
    fn the_summary_names_the_source_and_the_split() {
        let req = YoungGenSizing {
            xmn: Some(256 * M),
            ..Default::default()
        };
        let s = plan_young_gen(G, &req).expect("plan").summary();
        assert!(s.contains("256 MiB (-Xmn; 2 x 128 MiB semi-spaces)"), "{s}");
        assert!(s.contains("old generation 768 MiB"), "{s}");
    }

    /// gcj j3/yg3: the eden + survivor geometry inside the pair. `-Xmn128m`
    /// with HotSpot's defaults is DefNew's 12.75 MiB survivors and
    /// 102.5 MiB eden; the commit never exceeds the budget; the trigger the
    /// pair can then reach (eden) is HotSpot's, not 95 % of a 64 MiB semi.
    #[test]
    fn gcj_j3yg3_the_serial_pair_geometry_is_defnews_inside_the_budget() {
        const A: usize = 64 * 1024;
        let g = plan_serial_pair_geometry(128 * M, 8, A).expect("fits");
        assert_eq!(g.survivor, 13_369_344, "align_down(128 MiB / 10, 64 KiB)");
        assert_eq!(g.eden, 128 * M - 2 * 13_369_344);
        assert_eq!(g.semi_reserve, g.eden + g.survivor);
        assert_eq!(g.max_young_commit(), 128 * M, "the commit is the budget");
        assert!(g.eden > (64 * M) * 95 / 100, "above today's pair trigger");
        for (y, sr) in [(64 * M, 8), (256 * M, 1), (32 * M, 6), (G, 8), (3 * M + 5, 8)] {
            let g = plan_serial_pair_geometry(y, sr, A).expect("fits");
            assert_eq!(g.eden + 2 * g.survivor, g.young_budget, "{y} {sr}");
            assert!(g.young_budget <= y && g.survivor % A == 0, "{y} {sr}");
            assert_eq!(g.max_young_commit(), g.young_budget);
        }
        assert_eq!(plan_serial_pair_geometry(2 * A, 8, A), None, "no room for eden");
        assert_eq!(plan_serial_pair_geometry(128 * M, 8, 3), None, "not a power of two");
    }
}
