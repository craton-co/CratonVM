// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Metaspace management — separate memory region for class metadata.
//!
//! Models HotSpot's Metaspace: chunk-based allocation with per-classloader
//! accounting, compressed class space, and GC integration.
//!
//! # THIS IS A MODEL, NOT THE VM'S METASPACE
//!
//! Audit note, 2026-07-26 (`arch-2026-07-26/refs-metaspace-unloading`).
//!
//! Nothing in the workspace constructs a [`Metaspace`], a
//! [`MetaspaceRegistry`] or a [`CompressedClassSpace`]: a repo-wide search for
//! `Metaspace`/`metaspace::` outside this file matches only JFR event
//! descriptors, `-XX:` flag parsing that is accepted-and-ignored, unified-log
//! tag names, and the serviceability text report. So no class metadata is
//! actually allocated through this module, and its numbers are never the ones
//! a Java program observes.
//!
//! What the VM really reports as "metaspace" is an approximation computed in
//! `vm/src/vm/vm_init.rs` (search for "Metaspace approximation"): live class
//! count multiplied by an estimated per-class overhead. `native-builtins/src/jmx.rs`
//! documents the same thing from the `MemoryMXBean` side — CratonVM has no
//! per-pool non-heap accounting, so `-XX:MaxMetaspaceSize` does not bound
//! anything.
//!
//! Consequences worth being explicit about, because "metaspace is modelled"
//! reads as "metaspace is bounded" and it is not:
//!
//! * **Metaspace is unbounded in practice.** Class metadata lives in ordinary
//!   Rust allocations owned by `classloading::ClassStore` and friends. It
//!   shrinks when `vm/src/memory/gc.rs::unload_dead_class_metadata` runs (which
//!   it does — see `class_unloading.rs`'s module doc for that chain), but there
//!   is no cap and no `OutOfMemoryError: Metaspace`. A proxy-heavy workload
//!   (CGLIB / ByteBuddy, i.e. the Spring and Hibernate suites) is bounded only
//!   by loader unloading keeping up.
//! * The chunk allocator below hands out `(chunk_id, offset)` pairs, not
//!   pointers. It reserves no memory and cannot be made to.
//!
//! The model is kept because it is the natural home for real accounting and its
//! chunking/GC policy is already tested; see
//! `refs-metaspace-unloading.md` for the wiring
//! sketch and its prerequisites.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration knobs for the metaspace allocator.
#[derive(Debug, Clone)]
pub struct MetaspaceConfig {
    /// -XX:MaxMetaspaceSize (default: no limit)
    pub max_metaspace_size: usize,
    /// Initial chunk size for new class loaders (default: 256 KB)
    pub initial_chunk_size: usize,
    /// Small chunk size (default: 4 KB)
    pub small_chunk_size: usize,
    /// Medium chunk size (default: 64 KB)
    pub medium_chunk_size: usize,
    /// Trigger metaspace GC when used exceeds this (default: 20 MB)
    pub gc_threshold: usize,
    /// Minimum free ratio after GC (default: 0.4)
    pub min_free_ratio: f64,
    /// Maximum free ratio after GC (default: 0.7)
    pub max_free_ratio: f64,
}

impl Default for MetaspaceConfig {
    fn default() -> Self {
        Self {
            max_metaspace_size: usize::MAX,
            initial_chunk_size: 256 * 1024,
            small_chunk_size: 4096,
            medium_chunk_size: 64 * 1024,
            gc_threshold: 20 * 1024 * 1024,
            min_free_ratio: 0.4,
            max_free_ratio: 0.7,
        }
    }
}

/// Errors returned by [`MetaspaceConfig::validate`].
///
/// Surfaced via [`Metaspace::new`] (which returns `Result`) so a malformed
/// configuration is rejected at construction time rather than silently
/// distorting the GC-trigger heuristic that reads the ratios.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaspaceConfigError {
    /// `min_free_ratio > max_free_ratio` — the GC-after-collection target
    /// window is inverted; the trigger heuristic cannot satisfy both bounds.
    InvertedFreeRatio { min: f64, max: f64 },
    /// A free-ratio value is not within `[0.0, 1.0]` (or is NaN).
    RatioOutOfRange { name: &'static str, value: f64 },
    /// `small_chunk_size`, `medium_chunk_size`, or `initial_chunk_size` is
    /// zero, or the size tiers are not non-decreasing
    /// (`small <= medium <= initial`).
    InvalidChunkSizes {
        small: usize,
        medium: usize,
        initial: usize,
    },
}

impl std::fmt::Display for MetaspaceConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvertedFreeRatio { min, max } => write!(
                f,
                "MetaspaceConfig: min_free_ratio ({min}) > max_free_ratio ({max})"
            ),
            Self::RatioOutOfRange { name, value } => write!(
                f,
                "MetaspaceConfig: {name} = {value} is outside [0.0, 1.0]"
            ),
            Self::InvalidChunkSizes {
                small,
                medium,
                initial,
            } => write!(
                f,
                "MetaspaceConfig: chunk sizes invalid (small={small}, medium={medium}, initial={initial}); require 0 < small <= medium <= initial"
            ),
        }
    }
}

impl std::error::Error for MetaspaceConfigError {}

impl MetaspaceConfig {
    /// Validate the configuration. Returns `Err` if any bound is inverted,
    /// any ratio is out of `[0.0, 1.0]`, or the chunk-size tiers are not
    /// non-decreasing. Called automatically by [`Metaspace::new`].
    pub fn validate(&self) -> Result<(), MetaspaceConfigError> {
        // Ratios first: catch NaN via `!(0.0..=1.0).contains(&v)` (NaN
        // returns false for every comparison).
        if !(0.0..=1.0).contains(&self.min_free_ratio) {
            return Err(MetaspaceConfigError::RatioOutOfRange {
                name: "min_free_ratio",
                value: self.min_free_ratio,
            });
        }
        if !(0.0..=1.0).contains(&self.max_free_ratio) {
            return Err(MetaspaceConfigError::RatioOutOfRange {
                name: "max_free_ratio",
                value: self.max_free_ratio,
            });
        }
        if self.min_free_ratio > self.max_free_ratio {
            return Err(MetaspaceConfigError::InvertedFreeRatio {
                min: self.min_free_ratio,
                max: self.max_free_ratio,
            });
        }

        // Chunk sizes: every tier must be non-zero and non-decreasing.
        if self.small_chunk_size == 0
            || self.medium_chunk_size == 0
            || self.initial_chunk_size == 0
            || self.small_chunk_size > self.medium_chunk_size
            || self.medium_chunk_size > self.initial_chunk_size
        {
            return Err(MetaspaceConfigError::InvalidChunkSizes {
                small: self.small_chunk_size,
                medium: self.medium_chunk_size,
                initial: self.initial_chunk_size,
            });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chunk types & chunks
// ---------------------------------------------------------------------------

/// Classification of metaspace chunks by size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkType {
    Small,
    Medium,
    Large,
    Humongous,
}

/// A single metaspace chunk owned by a class loader.
#[derive(Debug, Clone)]
pub struct MetaspaceChunk {
    pub id: u64,
    pub size: usize,
    pub used: usize,
    pub owner_loader_id: u64,
    pub chunk_type: ChunkType,
    pub next_free_offset: usize,
    pub is_free: bool,
}

// ---------------------------------------------------------------------------
// Allocation result types
// ---------------------------------------------------------------------------

/// Successful allocation descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaspaceAllocation {
    pub chunk_id: u64,
    pub offset: usize,
    pub size: usize,
}

/// Errors returned by the metaspace allocator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaspaceError {
    OutOfMetaspace {
        requested: usize,
        available: usize,
        max: usize,
    },
    ChunkAllocationFailed {
        size: usize,
    },
}

impl std::fmt::Display for MetaspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMetaspace {
                requested,
                available,
                max,
            } => write!(
                f,
                "OutOfMetaspace: requested {requested}, available {available}, max {max}"
            ),
            Self::ChunkAllocationFailed { size } => {
                write!(f, "ChunkAllocationFailed: size {size}")
            }
        }
    }
}

/// Result of freeing a class loader's metaspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreedMetaspace {
    pub chunks_freed: usize,
    pub bytes_freed: usize,
    pub loader_id: u64,
}

/// Result of a metaspace GC cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaspaceGcResult {
    pub bytes_reclaimed: usize,
    pub chunks_freed: usize,
    pub chunks_merged: usize,
    pub time_ms: u64,
    pub new_threshold: usize,
}

/// Aggregate stats exposed for JMX / monitoring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaspaceStats {
    pub committed: usize,
    pub used: usize,
    pub capacity: usize,
    pub chunk_count: usize,
    pub free_chunk_count: usize,
    pub gc_count: u32,
    pub loader_count: usize,
    pub class_count: usize,
}

// ---------------------------------------------------------------------------
// Metaspace allocator
// ---------------------------------------------------------------------------

/// The main metaspace allocator.
#[derive(Debug)]
pub struct Metaspace {
    pub config: MetaspaceConfig,
    pub chunks: Vec<MetaspaceChunk>,
    pub total_capacity: usize,
    pub total_used: usize,
    pub high_water_mark: usize,
    pub next_chunk_id: u64,
    pub gc_count: u32,
    /// Fast-path cache for the hot allocation loop.
    ///
    /// Maps `loader_id -> index into self.chunks` of the **first** non-free
    /// chunk owned by that loader — i.e. the exact chunk the linear
    /// "fit-existing" scan in [`Metaspace::allocate`] lands on first. When a
    /// cached entry exists it is guaranteed to point at that first-owned
    /// chunk, so the common bump-allocate case becomes O(1) instead of an
    /// O(chunks) scan.
    ///
    /// Invariant maintenance (kept deliberately conservative so the fast path
    /// is a *pure* speedup — never a different allocation result):
    /// * only the slow fit-existing scan *sets* an entry, to the first
    ///   owned non-free chunk it encounters (independent of whether that
    ///   chunk has room);
    /// * a fast-path hit leaves the entry untouched (index unchanged);
    /// * the reuse-free and new-chunk paths *remove* the loader's entry, since
    ///   the freshly claimed/appended chunk may not be the loader's first
    ///   owned chunk — the next allocate re-establishes the truth via a scan;
    /// * any structural mutation of `chunks` ([`Metaspace::free_loader_metaspace`],
    ///   [`Metaspace::trigger_gc`]) clears the whole cache.
    ///
    /// Not part of the public allocation contract; purely an internal index.
    active_chunk: FxHashMap<u64, usize>,
}

impl Metaspace {
    /// Construct a `Metaspace` with the given configuration.
    ///
    /// Returns `Err(MetaspaceConfigError)` if the configuration fails
    /// validation (inverted free-ratio bounds, ratio outside `[0.0, 1.0]`,
    /// zero / non-monotone chunk sizes). Callers using the canonical
    /// `MetaspaceConfig::default()` may prefer [`Metaspace::with_default`]
    /// for the common case where validation cannot fail.
    pub fn new(config: MetaspaceConfig) -> Result<Self, MetaspaceConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            chunks: Vec::new(),
            total_capacity: 0,
            total_used: 0,
            high_water_mark: 0,
            next_chunk_id: 1,
            gc_count: 0,
            active_chunk: FxHashMap::default(),
        })
    }

    /// Convenience constructor using [`MetaspaceConfig::default`].
    ///
    /// The default config is statically known to pass validation, so this
    /// constructor is infallible (`.unwrap()`-free at call sites). Prefer
    /// [`Metaspace::new`] when callers need explicit config control and
    /// must handle the validation error path.
    pub fn with_default() -> Self {
        // The Default impl is engineered to always validate. If a future
        // edit breaks that invariant this expect will fire at first use.
        Self::new(MetaspaceConfig::default())
            .expect("MetaspaceConfig::default() must always validate")
    }

    /// Determine chunk type from size.
    fn chunk_type_for_size(&self, size: usize) -> ChunkType {
        if size <= self.config.small_chunk_size {
            ChunkType::Small
        } else if size <= self.config.medium_chunk_size {
            ChunkType::Medium
        } else if size <= self.config.initial_chunk_size {
            ChunkType::Large
        } else {
            ChunkType::Humongous
        }
    }

    /// Pick an appropriate chunk size for the requested allocation.
    fn pick_chunk_size(&self, requested: usize) -> usize {
        if requested <= self.config.small_chunk_size {
            self.config.small_chunk_size
        } else if requested <= self.config.medium_chunk_size {
            self.config.medium_chunk_size
        } else if requested <= self.config.initial_chunk_size {
            self.config.initial_chunk_size
        } else {
            // humongous — allocate exactly what is needed
            requested
        }
    }

    /// Allocate `size` bytes for `loader_id`.
    pub fn allocate(
        &mut self,
        size: usize,
        loader_id: u64,
    ) -> Result<MetaspaceAllocation, MetaspaceError> {
        // Fast path: bump-allocate from this loader's cached active chunk.
        //
        // The cache, when present, points at the first non-free chunk owned by
        // `loader_id` — the exact chunk the linear scan below would select
        // first. If that chunk still has room the result is byte-for-byte the
        // same as the scan would produce, so this is a pure O(1) speedup.
        if let Some(&idx) = self.active_chunk.get(&loader_id) {
            if let Some(chunk) = self.chunks.get_mut(idx) {
                // Guard against a stale entry (defence in depth — the
                // invalidation rules below already keep this exact).
                if chunk.owner_loader_id == loader_id && !chunk.is_free {
                    let remaining = chunk.size - chunk.next_free_offset;
                    if remaining >= size {
                        let offset = chunk.next_free_offset;
                        chunk.next_free_offset += size;
                        chunk.used += size;
                        self.total_used += size;
                        if self.total_used > self.high_water_mark {
                            self.high_water_mark = self.total_used;
                        }
                        return Ok(MetaspaceAllocation {
                            chunk_id: chunk.id,
                            offset,
                            size,
                        });
                    }
                }
            }
        }

        // Slow path: linear "fit-existing" scan. While scanning, record the
        // index of the first non-free chunk owned by this loader (independent
        // of whether it has room) so the next allocation can take the fast
        // path. This preserves the original first-fit selection exactly.
        let mut first_owned: Option<usize> = None;
        for (idx, chunk) in self.chunks.iter_mut().enumerate() {
            if chunk.owner_loader_id == loader_id && !chunk.is_free {
                if first_owned.is_none() {
                    first_owned = Some(idx);
                }
                let remaining = chunk.size - chunk.next_free_offset;
                if remaining >= size {
                    let offset = chunk.next_free_offset;
                    chunk.next_free_offset += size;
                    chunk.used += size;
                    self.total_used += size;
                    if self.total_used > self.high_water_mark {
                        self.high_water_mark = self.total_used;
                    }
                    // `first_owned` is guaranteed set here (this chunk is owned
                    // & non-free), and points at the loader's first owned chunk.
                    if let Some(first) = first_owned {
                        self.active_chunk.insert(loader_id, first);
                    }
                    return Ok(MetaspaceAllocation {
                        chunk_id: chunk.id,
                        offset,
                        size,
                    });
                }
            }
        }
        // No owned chunk had room. If the loader owns a first non-free chunk,
        // cache it so a future smaller allocation can hit the fast path.
        if let Some(first) = first_owned {
            self.active_chunk.insert(loader_id, first);
        }

        // Try to reuse a free chunk that fits.
        let chunk_size_needed = self.pick_chunk_size(size);
        for chunk in self.chunks.iter_mut() {
            if chunk.is_free && chunk.size >= chunk_size_needed {
                chunk.is_free = false;
                chunk.owner_loader_id = loader_id;
                chunk.used = size;
                chunk.next_free_offset = size;
                self.total_used += size;
                if self.total_used > self.high_water_mark {
                    self.high_water_mark = self.total_used;
                }
                // The reclaimed chunk may sit before the loader's previously
                // cached first-owned chunk; drop the entry so the next
                // allocation re-derives the true first-owned index via a scan.
                self.active_chunk.remove(&loader_id);
                return Ok(MetaspaceAllocation {
                    chunk_id: chunk.id,
                    offset: 0,
                    size,
                });
            }
        }

        // Need a new chunk — check max.
        let new_chunk_size = self.pick_chunk_size(size);
        if self.total_capacity + new_chunk_size > self.config.max_metaspace_size {
            return Err(MetaspaceError::OutOfMetaspace {
                requested: size,
                available: self
                    .config
                    .max_metaspace_size
                    .saturating_sub(self.total_capacity),
                max: self.config.max_metaspace_size,
            });
        }

        let chunk_id = self.next_chunk_id;
        self.next_chunk_id += 1;
        let ctype = self.chunk_type_for_size(new_chunk_size);
        let chunk = MetaspaceChunk {
            id: chunk_id,
            size: new_chunk_size,
            used: size,
            owner_loader_id: loader_id,
            chunk_type: ctype,
            next_free_offset: size,
            is_free: false,
        };
        self.chunks.push(chunk);
        self.total_capacity += new_chunk_size;
        self.total_used += size;
        if self.total_used > self.high_water_mark {
            self.high_water_mark = self.total_used;
        }
        // A loader that already owned (full) chunks keeps them ahead of this
        // freshly appended one, so the new chunk is not necessarily the first
        // owned chunk. Drop the entry; the next allocation re-derives it.
        self.active_chunk.remove(&loader_id);

        Ok(MetaspaceAllocation {
            chunk_id,
            offset: 0,
            size,
        })
    }

    /// Free all metaspace owned by a class loader (called when the loader is GC'd).
    pub fn free_loader_metaspace(&mut self, loader_id: u64) -> FreedMetaspace {
        let mut chunks_freed = 0usize;
        let mut bytes_freed = 0usize;

        for chunk in self.chunks.iter_mut() {
            if chunk.owner_loader_id == loader_id && !chunk.is_free {
                bytes_freed += chunk.used;
                chunk.is_free = true;
                chunk.used = 0;
                chunk.next_free_offset = 0;
                chunks_freed += 1;
            }
        }
        self.total_used = self.total_used.saturating_sub(bytes_freed);

        // Freeing chunks changes which (if any) chunk is each loader's first
        // owned non-free chunk; drop the whole fast-path cache. (Indices are
        // unchanged here — chunks are only flagged, not removed — but the
        // freed loader's first-owned chunk is now gone, so a blanket clear is
        // the simplest correct choice and freeing is far off the hot path.)
        self.active_chunk.clear();

        FreedMetaspace {
            chunks_freed,
            bytes_freed,
            loader_id,
        }
    }

    /// Should we trigger a metaspace GC?
    pub fn should_trigger_gc(&self) -> bool {
        self.total_used >= self.config.gc_threshold
    }

    /// Trigger a GC cycle: reclaim free chunks and merge adjacent small free chunks.
    pub fn trigger_gc(&mut self) -> MetaspaceGcResult {
        let start = std::time::Instant::now();
        self.gc_count += 1;

        // GC removes and coalesces chunks, shifting every index; the
        // fast-path cache (which stores `chunks` indices) is no longer valid.
        self.active_chunk.clear();

        let mut bytes_reclaimed = 0usize;
        let mut chunks_freed = 0usize;
        let mut chunks_merged = 0usize;

        // Remove free chunks, reclaiming their capacity.
        self.chunks.retain(|c| {
            if c.is_free {
                bytes_reclaimed += c.size;
                chunks_freed += 1;
                false
            } else {
                true
            }
        });
        self.total_capacity = self.total_capacity.saturating_sub(bytes_reclaimed);

        // Merge: coalesce consecutive same-loader chunks with remaining space.
        // Simple heuristic: if two consecutive chunks belong to the same loader and
        // the first has free space, merge the second into the first.
        let mut i = 0;
        while i + 1 < self.chunks.len() {
            let same_loader = self.chunks[i].owner_loader_id == self.chunks[i + 1].owner_loader_id;
            let first_has_space = self.chunks[i].next_free_offset < self.chunks[i].size;
            if same_loader
                && first_has_space
                && !self.chunks[i].is_free
                && !self.chunks[i + 1].is_free
            {
                let second_used = self.chunks[i + 1].used;
                let second_size = self.chunks[i + 1].size;
                self.chunks[i].size += second_size;
                self.chunks[i].used += second_used;
                self.chunks[i].next_free_offset += second_used;
                self.chunks.remove(i + 1);
                chunks_merged += 1;
            } else {
                i += 1;
            }
        }

        // Adjust threshold based on free ratio.
        let free_ratio = if self.total_capacity > 0 {
            1.0 - (self.total_used as f64 / self.total_capacity as f64)
        } else {
            1.0
        };

        let new_threshold = if free_ratio < self.config.min_free_ratio {
            // Too little free space — raise threshold to allow more allocation before next GC.
            (self.total_used as f64 * 1.5) as usize
        } else if free_ratio > self.config.max_free_ratio {
            // Lots of free space — lower threshold.
            (self.total_used as f64 * 1.2) as usize
        } else {
            self.config.gc_threshold
        };
        let new_threshold = new_threshold.max(1);
        self.config.gc_threshold = new_threshold;

        let elapsed = start.elapsed();

        MetaspaceGcResult {
            bytes_reclaimed,
            chunks_freed,
            chunks_merged,
            time_ms: elapsed.as_millis() as u64,
            new_threshold,
        }
    }

    /// Aggregate statistics.
    pub fn get_stats(&self) -> MetaspaceStats {
        let free_chunk_count = self.chunks.iter().filter(|c| c.is_free).count();
        MetaspaceStats {
            committed: self.total_capacity,
            used: self.total_used,
            capacity: self.config.max_metaspace_size,
            chunk_count: self.chunks.len(),
            free_chunk_count,
            gc_count: self.gc_count,
            loader_count: 0, // filled externally via registry
            class_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-ClassLoader accounting
// ---------------------------------------------------------------------------

/// Per-classloader metaspace bookkeeping.
#[derive(Debug, Clone)]
pub struct ClassLoaderMetaspace {
    pub loader_id: u64,
    pub loader_name: String,
    pub allocated_bytes: usize,
    pub chunk_count: usize,
    pub class_count: usize,
    pub is_alive: bool,
}

/// Registry tracking all known class loaders.
/// T10.9.B: FxHashMap — loader_id is internal.
#[derive(Debug, Default)]
pub struct MetaspaceRegistry {
    pub loaders: FxHashMap<u64, ClassLoaderMetaspace>,
}

impl MetaspaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_loader(&mut self, id: u64, name: String) {
        self.loaders.insert(
            id,
            ClassLoaderMetaspace {
                loader_id: id,
                loader_name: name,
                allocated_bytes: 0,
                chunk_count: 0,
                class_count: 0,
                is_alive: true,
            },
        );
    }

    pub fn record_allocation(&mut self, loader_id: u64, bytes: usize) {
        if let Some(loader) = self.loaders.get_mut(&loader_id) {
            loader.allocated_bytes += bytes;
            loader.chunk_count += 1;
        }
    }

    pub fn record_class_loaded(&mut self, loader_id: u64) {
        if let Some(loader) = self.loaders.get_mut(&loader_id) {
            loader.class_count += 1;
        }
    }

    /// Flag a loader dead without dropping its accounting row.
    ///
    /// The row is retained deliberately so [`Self::dead_loaders`] and
    /// [`Self::get_all_stats`] can report it once — but it MUST be released
    /// afterwards. Use [`Self::mark_dead`] + [`Self::prune_dead`], or
    /// [`Self::remove_loader`] when no reporting pass is needed.
    pub fn mark_dead(&mut self, loader_id: u64) {
        if let Some(loader) = self.loaders.get_mut(&loader_id) {
            loader.is_alive = false;
        }
    }

    /// Drop a loader's accounting row outright. Returns the removed row.
    ///
    /// BOUNDED-METADATA FIX (2026-07-26): before this existed, `mark_dead` was
    /// the ONLY unload-side operation and it merely flipped `is_alive`. Since
    /// `register_loader` is the only other writer, `loaders` was append-only:
    /// every loader ever created kept a `ClassLoaderMetaspace` row (with an
    /// owned `loader_name: String`) for the process lifetime. That is the exact
    /// shape of leak `docs/architecture/class-loader-unloading.md`
    /// forbids ("either unload invalidation or hard bounds"), and it is invisible
    /// — the row is small, so it fails only after hours under a proxy-generating
    /// workload.
    pub fn remove_loader(&mut self, loader_id: u64) -> Option<ClassLoaderMetaspace> {
        self.loaders.remove(&loader_id)
    }

    /// Release every row already flagged dead. Returns how many were released.
    ///
    /// Call this at the end of each unload transaction, after any pass that
    /// wants to observe the dead set.
    pub fn prune_dead(&mut self) -> usize {
        let before = self.loaders.len();
        self.loaders.retain(|_, l| l.is_alive);
        before - self.loaders.len()
    }

    pub fn get_loader_stats(&self, loader_id: u64) -> Option<&ClassLoaderMetaspace> {
        self.loaders.get(&loader_id)
    }

    pub fn get_all_stats(&self) -> Vec<&ClassLoaderMetaspace> {
        self.loaders.values().collect()
    }

    pub fn total_loaders(&self) -> usize {
        self.loaders.len()
    }

    pub fn dead_loaders(&self) -> usize {
        self.loaders.values().filter(|l| !l.is_alive).count()
    }
}

// ---------------------------------------------------------------------------
// Compressed Class Space
// ---------------------------------------------------------------------------

/// Simulated compressed class pointer space (like -XX:CompressedClassSpaceSize).
#[derive(Debug)]
pub struct CompressedClassSpace {
    pub base_address: usize,
    pub reserved_size: usize,
    pub committed_size: usize,
    pub used_size: usize,
    pub max_size: usize,
}

/// Stats for the compressed class space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedClassSpaceStats {
    pub reserved: usize,
    pub committed: usize,
    pub used: usize,
    pub free: usize,
}

impl CompressedClassSpace {
    /// Create with default 1 GB reserved.
    pub fn new(base_address: usize, max_size: usize) -> Self {
        Self {
            base_address,
            reserved_size: max_size,
            committed_size: 0,
            used_size: 0,
            max_size,
        }
    }

    /// Allocate `size` bytes, returning the offset from `base_address`.
    pub fn allocate(&mut self, size: usize) -> Result<usize, MetaspaceError> {
        if self.used_size + size > self.max_size {
            return Err(MetaspaceError::OutOfMetaspace {
                requested: size,
                available: self.max_size.saturating_sub(self.used_size),
                max: self.max_size,
            });
        }
        let offset = self.used_size;
        self.used_size += size;
        // Commit in 64 KB pages.
        let commit_granularity = 64 * 1024;
        while self.committed_size < self.used_size {
            self.committed_size += commit_granularity;
        }
        if self.committed_size > self.max_size {
            self.committed_size = self.max_size;
        }
        Ok(offset)
    }

    pub fn get_stats(&self) -> CompressedClassSpaceStats {
        CompressedClassSpaceStats {
            reserved: self.reserved_size,
            committed: self.committed_size,
            used: self.used_size,
            free: self.max_size.saturating_sub(self.used_size),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- MetaspaceConfig defaults -------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = MetaspaceConfig::default();
        assert_eq!(cfg.max_metaspace_size, usize::MAX);
        assert_eq!(cfg.initial_chunk_size, 256 * 1024);
        assert_eq!(cfg.small_chunk_size, 4096);
        assert_eq!(cfg.medium_chunk_size, 64 * 1024);
        assert_eq!(cfg.gc_threshold, 20 * 1024 * 1024);
        assert!((cfg.min_free_ratio - 0.4).abs() < f64::EPSILON);
        assert!((cfg.max_free_ratio - 0.7).abs() < f64::EPSILON);
    }

    // -- Basic allocation ---------------------------------------------------

    #[test]
    fn allocate_small() {
        let mut ms = Metaspace::with_default();
        let alloc = ms.allocate(128, 1).unwrap();
        assert_eq!(alloc.offset, 0);
        assert_eq!(alloc.size, 128);
        assert_eq!(ms.total_used, 128);
        assert_eq!(ms.chunks.len(), 1);
    }

    #[test]
    fn allocate_fits_existing_chunk() {
        let mut ms = Metaspace::with_default();
        let a1 = ms.allocate(100, 1).unwrap();
        let a2 = ms.allocate(200, 1).unwrap();
        // Should reuse the same chunk.
        assert_eq!(a1.chunk_id, a2.chunk_id);
        assert_eq!(a2.offset, 100);
        assert_eq!(ms.total_used, 300);
        assert_eq!(ms.chunks.len(), 1);
    }

    #[test]
    fn allocate_new_chunk_when_full() {
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 256;
        let mut ms = Metaspace::new(cfg).unwrap();
        let a1 = ms.allocate(256, 1).unwrap(); // fills the small chunk
        let a2 = ms.allocate(64, 1).unwrap(); // needs a new chunk
        assert_ne!(a1.chunk_id, a2.chunk_id);
        assert_eq!(ms.chunks.len(), 2);
    }

    #[test]
    fn allocate_different_loaders_get_different_chunks() {
        let mut ms = Metaspace::with_default();
        let a1 = ms.allocate(64, 1).unwrap();
        let a2 = ms.allocate(64, 2).unwrap();
        assert_ne!(a1.chunk_id, a2.chunk_id);
    }

    #[test]
    fn allocate_medium_chunk() {
        let mut ms = Metaspace::with_default();
        let alloc = ms.allocate(8192, 1).unwrap();
        assert_eq!(alloc.offset, 0);
        assert_eq!(ms.chunks[0].chunk_type, ChunkType::Medium);
    }

    #[test]
    fn allocate_large_chunk() {
        let mut ms = Metaspace::with_default();
        let alloc = ms.allocate(128 * 1024, 1).unwrap();
        assert_eq!(alloc.offset, 0);
        assert_eq!(ms.chunks[0].chunk_type, ChunkType::Large);
    }

    #[test]
    fn allocate_humongous_chunk() {
        let mut ms = Metaspace::with_default();
        let alloc = ms.allocate(512 * 1024, 1).unwrap();
        assert_eq!(alloc.offset, 0);
        assert_eq!(ms.chunks[0].chunk_type, ChunkType::Humongous);
        assert_eq!(ms.chunks[0].size, 512 * 1024);
    }

    // -- Max metaspace enforcement ------------------------------------------

    #[test]
    fn allocate_exceeds_max_metaspace() {
        let mut cfg = MetaspaceConfig::default();
        cfg.max_metaspace_size = 1024;
        cfg.small_chunk_size = 512;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(256, 1).unwrap(); // 512-byte chunk
        ms.allocate(256, 2).unwrap(); // 512-byte chunk -> total_capacity = 1024
        let err = ms.allocate(256, 3).unwrap_err();
        match err {
            MetaspaceError::OutOfMetaspace { requested, max, .. } => {
                assert_eq!(requested, 256);
                assert_eq!(max, 1024);
            }
            _ => panic!("expected OutOfMetaspace"),
        }
    }

    #[test]
    fn allocate_exact_max() {
        let mut cfg = MetaspaceConfig::default();
        cfg.max_metaspace_size = 4096;
        cfg.small_chunk_size = 4096;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(4096, 1).unwrap();
        assert_eq!(ms.total_capacity, 4096);
    }

    // -- Free loader metaspace ----------------------------------------------

    #[test]
    fn free_loader_metaspace_basic() {
        let mut ms = Metaspace::with_default();
        ms.allocate(512, 1).unwrap();
        ms.allocate(256, 1).unwrap();
        let freed = ms.free_loader_metaspace(1);
        assert_eq!(freed.loader_id, 1);
        assert!(freed.bytes_freed > 0);
        assert_eq!(ms.total_used, 0);
        // Chunks still exist but marked free.
        assert!(ms.chunks.iter().all(|c| c.is_free));
    }

    #[test]
    fn free_loader_does_not_affect_other_loaders() {
        let mut ms = Metaspace::with_default();
        ms.allocate(512, 1).unwrap();
        ms.allocate(256, 2).unwrap();
        ms.free_loader_metaspace(1);
        // Loader 2's data should still be used.
        assert_eq!(ms.total_used, 256);
    }

    #[test]
    fn free_nonexistent_loader_is_noop() {
        let mut ms = Metaspace::with_default();
        ms.allocate(128, 1).unwrap();
        let freed = ms.free_loader_metaspace(999);
        assert_eq!(freed.chunks_freed, 0);
        assert_eq!(freed.bytes_freed, 0);
    }

    // -- Reuse free chunks --------------------------------------------------

    #[test]
    fn reuse_freed_chunk() {
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 4096;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(1024, 1).unwrap();
        ms.free_loader_metaspace(1);
        // Now allocate for loader 2 — should reuse the free chunk.
        let a = ms.allocate(512, 2).unwrap();
        assert_eq!(ms.chunks.len(), 1);
        assert_eq!(a.offset, 0);
        assert_eq!(ms.chunks[0].owner_loader_id, 2);
    }

    // -- GC triggering ------------------------------------------------------

    #[test]
    fn should_trigger_gc_below_threshold() {
        let mut cfg = MetaspaceConfig::default();
        cfg.gc_threshold = 1024;
        let ms = Metaspace::new(cfg).unwrap();
        assert!(!ms.should_trigger_gc());
    }

    #[test]
    fn should_trigger_gc_at_threshold() {
        let mut cfg = MetaspaceConfig::default();
        cfg.gc_threshold = 256;
        cfg.small_chunk_size = 4096;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(256, 1).unwrap();
        assert!(ms.should_trigger_gc());
    }

    #[test]
    fn trigger_gc_reclaims_free_chunks() {
        let mut ms = Metaspace::with_default();
        ms.allocate(512, 1).unwrap();
        ms.allocate(256, 2).unwrap();
        ms.free_loader_metaspace(1);
        let result = ms.trigger_gc();
        assert!(result.chunks_freed > 0);
        assert!(result.bytes_reclaimed > 0);
        assert_eq!(ms.gc_count, 1);
        // Only loader 2's chunk should remain.
        assert_eq!(ms.chunks.len(), 1);
    }

    #[test]
    fn trigger_gc_updates_threshold() {
        let mut cfg = MetaspaceConfig::default();
        cfg.gc_threshold = 100;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(64, 1).unwrap();
        ms.free_loader_metaspace(1);
        let result = ms.trigger_gc();
        // Threshold should be updated (all free, so everything reclaimed).
        assert!(result.new_threshold > 0);
    }

    #[test]
    fn trigger_gc_increments_count() {
        let mut ms = Metaspace::with_default();
        ms.trigger_gc();
        ms.trigger_gc();
        assert_eq!(ms.gc_count, 2);
    }

    #[test]
    fn trigger_gc_merges_consecutive_same_loader_chunks() {
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 128;
        let mut ms = Metaspace::new(cfg).unwrap();
        // Force two chunks for same loader by filling the first.
        ms.allocate(128, 1).unwrap();
        ms.allocate(64, 1).unwrap();
        assert_eq!(ms.chunks.len(), 2);
        // Both belong to loader 1, first is full but let's pretend it has space by
        // adjusting next_free_offset < size.
        ms.chunks[0].next_free_offset = 64; // simulate partial use
        let result = ms.trigger_gc();
        assert!(result.chunks_merged > 0);
    }

    // -- High water mark ----------------------------------------------------

    #[test]
    fn high_water_mark_tracks_peak() {
        let mut ms = Metaspace::with_default();
        ms.allocate(1024, 1).unwrap();
        ms.allocate(2048, 2).unwrap();
        let peak = ms.high_water_mark;
        ms.free_loader_metaspace(2);
        assert_eq!(ms.high_water_mark, peak); // should not decrease
    }

    // -- Stats --------------------------------------------------------------

    #[test]
    fn get_stats_basic() {
        let mut ms = Metaspace::with_default();
        ms.allocate(256, 1).unwrap();
        let stats = ms.get_stats();
        assert!(stats.committed > 0);
        assert_eq!(stats.used, 256);
        assert_eq!(stats.capacity, usize::MAX);
        assert_eq!(stats.chunk_count, 1);
        assert_eq!(stats.free_chunk_count, 0);
        assert_eq!(stats.gc_count, 0);
    }

    #[test]
    fn get_stats_with_free_chunks() {
        let mut ms = Metaspace::with_default();
        ms.allocate(128, 1).unwrap();
        ms.allocate(128, 2).unwrap();
        ms.free_loader_metaspace(1);
        let stats = ms.get_stats();
        assert_eq!(stats.free_chunk_count, 1);
    }

    // -- ChunkType classification -------------------------------------------

    #[test]
    fn chunk_type_classification() {
        let ms = Metaspace::with_default();
        assert_eq!(ms.chunk_type_for_size(1024), ChunkType::Small);
        assert_eq!(ms.chunk_type_for_size(4096), ChunkType::Small);
        assert_eq!(ms.chunk_type_for_size(8192), ChunkType::Medium);
        assert_eq!(ms.chunk_type_for_size(64 * 1024), ChunkType::Medium);
        assert_eq!(ms.chunk_type_for_size(128 * 1024), ChunkType::Large);
        assert_eq!(ms.chunk_type_for_size(256 * 1024), ChunkType::Large);
        assert_eq!(ms.chunk_type_for_size(512 * 1024), ChunkType::Humongous);
    }

    // -- MetaspaceError display ---------------------------------------------

    #[test]
    fn error_display() {
        let e = MetaspaceError::OutOfMetaspace {
            requested: 100,
            available: 50,
            max: 200,
        };
        let s = format!("{e}");
        assert!(s.contains("100"));
        assert!(s.contains("50"));
        assert!(s.contains("200"));

        let e2 = MetaspaceError::ChunkAllocationFailed { size: 42 };
        assert!(format!("{e2}").contains("42"));
    }

    // -- ClassLoaderMetaspace / Registry ------------------------------------

    #[test]
    fn registry_register_and_lookup() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "bootstrap".into());
        let stats = reg.get_loader_stats(1).unwrap();
        assert_eq!(stats.loader_name, "bootstrap");
        assert!(stats.is_alive);
        assert_eq!(stats.class_count, 0);
    }

    #[test]
    fn registry_record_allocation() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "app".into());
        reg.record_allocation(1, 4096);
        reg.record_allocation(1, 2048);
        let stats = reg.get_loader_stats(1).unwrap();
        assert_eq!(stats.allocated_bytes, 6144);
        assert_eq!(stats.chunk_count, 2);
    }

    #[test]
    fn registry_record_class_loaded() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "app".into());
        reg.record_class_loaded(1);
        reg.record_class_loaded(1);
        reg.record_class_loaded(1);
        assert_eq!(reg.get_loader_stats(1).unwrap().class_count, 3);
    }

    #[test]
    fn registry_mark_dead() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "app".into());
        reg.mark_dead(1);
        assert!(!reg.get_loader_stats(1).unwrap().is_alive);
    }

    #[test]
    fn registry_total_and_dead_loaders() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "bootstrap".into());
        reg.register_loader(2, "app".into());
        reg.register_loader(3, "web".into());
        reg.mark_dead(2);
        assert_eq!(reg.total_loaders(), 3);
        assert_eq!(reg.dead_loaders(), 1);
    }

    #[test]
    fn registry_get_all_stats() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "a".into());
        reg.register_loader(2, "b".into());
        let all = reg.get_all_stats();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn registry_unknown_loader_operations_are_noop() {
        let mut reg = MetaspaceRegistry::new();
        reg.record_allocation(999, 100);
        reg.record_class_loaded(999);
        reg.mark_dead(999);
        assert!(reg.get_loader_stats(999).is_none());
    }

    // -- CompressedClassSpace -----------------------------------------------

    #[test]
    fn compressed_class_space_allocate() {
        let mut ccs = CompressedClassSpace::new(0x800000000, 1024 * 1024);
        let off = ccs.allocate(256).unwrap();
        assert_eq!(off, 0);
        assert_eq!(ccs.used_size, 256);
    }

    #[test]
    fn compressed_class_space_sequential_allocations() {
        let mut ccs = CompressedClassSpace::new(0, 1024 * 1024);
        let o1 = ccs.allocate(100).unwrap();
        let o2 = ccs.allocate(200).unwrap();
        let o3 = ccs.allocate(300).unwrap();
        assert_eq!(o1, 0);
        assert_eq!(o2, 100);
        assert_eq!(o3, 300);
        assert_eq!(ccs.used_size, 600);
    }

    #[test]
    fn compressed_class_space_exceeds_max() {
        let mut ccs = CompressedClassSpace::new(0, 512);
        ccs.allocate(256).unwrap();
        let err = ccs.allocate(512).unwrap_err();
        match err {
            MetaspaceError::OutOfMetaspace { requested, max, .. } => {
                assert_eq!(requested, 512);
                assert_eq!(max, 512);
            }
            _ => panic!("expected OutOfMetaspace"),
        }
    }

    #[test]
    fn compressed_class_space_commit_granularity() {
        let mut ccs = CompressedClassSpace::new(0, 1024 * 1024);
        ccs.allocate(100).unwrap();
        // committed_size should be rounded up to 64KB granularity.
        assert!(ccs.committed_size >= ccs.used_size);
        assert_eq!(ccs.committed_size % (64 * 1024), 0);
    }

    #[test]
    fn compressed_class_space_stats() {
        let mut ccs = CompressedClassSpace::new(0, 1024 * 1024);
        ccs.allocate(1000).unwrap();
        let stats = ccs.get_stats();
        assert_eq!(stats.reserved, 1024 * 1024);
        assert_eq!(stats.used, 1000);
        assert!(stats.committed >= 1000);
        assert_eq!(stats.free, 1024 * 1024 - 1000);
    }

    #[test]
    fn compressed_class_space_fill_to_max() {
        let max = 64 * 1024;
        let mut ccs = CompressedClassSpace::new(0, max);
        // Allocate the entire space.
        ccs.allocate(max).unwrap();
        assert_eq!(ccs.used_size, max);
        // Next allocation should fail.
        assert!(ccs.allocate(1).is_err());
    }

    // -- Integration: Metaspace + Registry ----------------------------------

    #[test]
    fn integration_allocate_and_track() {
        let mut ms = Metaspace::with_default();
        let mut reg = MetaspaceRegistry::new();

        reg.register_loader(1, "bootstrap".into());
        let alloc = ms.allocate(2048, 1).unwrap();
        reg.record_allocation(1, alloc.size);
        reg.record_class_loaded(1);

        let loader_stats = reg.get_loader_stats(1).unwrap();
        assert_eq!(loader_stats.allocated_bytes, 2048);
        assert_eq!(loader_stats.class_count, 1);
        assert_eq!(ms.total_used, 2048);
    }

    #[test]
    fn integration_loader_gc_cycle() {
        let mut ms = Metaspace::with_default();
        let mut reg = MetaspaceRegistry::new();

        reg.register_loader(1, "webapp".into());
        ms.allocate(4096, 1).unwrap();
        reg.record_allocation(1, 4096);

        // Loader dies.
        reg.mark_dead(1);
        let freed = ms.free_loader_metaspace(1);
        assert_eq!(freed.bytes_freed, 4096);

        // GC reclaims chunks.
        let gc = ms.trigger_gc();
        assert!(gc.chunks_freed > 0);
        assert_eq!(ms.chunks.len(), 0);
    }

    #[test]
    fn integration_multiple_loaders_partial_gc() {
        let mut cfg = MetaspaceConfig::default();
        cfg.gc_threshold = 1024;
        let mut ms = Metaspace::new(cfg).unwrap();
        let mut reg = MetaspaceRegistry::new();

        reg.register_loader(1, "boot".into());
        reg.register_loader(2, "app".into());
        reg.register_loader(3, "plugin".into());

        ms.allocate(256, 1).unwrap();
        ms.allocate(256, 2).unwrap();
        ms.allocate(256, 3).unwrap();

        // Only loader 3 dies.
        reg.mark_dead(3);
        ms.free_loader_metaspace(3);

        let gc = ms.trigger_gc();
        assert_eq!(gc.chunks_freed, 1);
        // Loaders 1 and 2 still have their chunks.
        assert_eq!(ms.chunks.len(), 2);
    }

    // -- Edge cases ---------------------------------------------------------

    #[test]
    fn allocate_zero_size() {
        let mut ms = Metaspace::with_default();
        let alloc = ms.allocate(0, 1).unwrap();
        assert_eq!(alloc.size, 0);
    }

    #[test]
    fn free_loader_twice_is_safe() {
        let mut ms = Metaspace::with_default();
        ms.allocate(128, 1).unwrap();
        ms.free_loader_metaspace(1);
        let freed = ms.free_loader_metaspace(1);
        assert_eq!(freed.bytes_freed, 0);
    }

    #[test]
    fn gc_on_empty_metaspace() {
        let mut ms = Metaspace::with_default();
        let result = ms.trigger_gc();
        assert_eq!(result.chunks_freed, 0);
        assert_eq!(result.bytes_reclaimed, 0);
    }

    // -- MetaspaceConfig validation ----------------------------------------

    #[test]
    fn config_default_validates() {
        assert!(MetaspaceConfig::default().validate().is_ok());
    }

    #[test]
    fn config_inverted_free_ratio_rejected() {
        let cfg = MetaspaceConfig {
            min_free_ratio: 0.9,
            max_free_ratio: 0.4,
            ..MetaspaceConfig::default()
        };
        match Metaspace::new(cfg).unwrap_err() {
            MetaspaceConfigError::InvertedFreeRatio { .. } => {}
            other => panic!("expected InvertedFreeRatio, got {other:?}"),
        }
    }

    #[test]
    fn config_ratio_out_of_range_rejected() {
        let cfg = MetaspaceConfig {
            min_free_ratio: -0.1,
            ..MetaspaceConfig::default()
        };
        assert!(matches!(
            Metaspace::new(cfg).unwrap_err(),
            MetaspaceConfigError::RatioOutOfRange {
                name: "min_free_ratio",
                ..
            }
        ));

        let cfg2 = MetaspaceConfig {
            max_free_ratio: 1.5,
            ..MetaspaceConfig::default()
        };
        assert!(matches!(
            Metaspace::new(cfg2).unwrap_err(),
            MetaspaceConfigError::RatioOutOfRange {
                name: "max_free_ratio",
                ..
            }
        ));
    }

    #[test]
    fn config_nan_ratio_rejected() {
        let cfg = MetaspaceConfig {
            min_free_ratio: f64::NAN,
            ..MetaspaceConfig::default()
        };
        assert!(matches!(
            Metaspace::new(cfg).unwrap_err(),
            MetaspaceConfigError::RatioOutOfRange { .. }
        ));
    }

    #[test]
    fn config_inverted_chunk_sizes_rejected() {
        let cfg = MetaspaceConfig {
            small_chunk_size: 8192,
            medium_chunk_size: 4096,
            ..MetaspaceConfig::default()
        };
        assert!(matches!(
            Metaspace::new(cfg).unwrap_err(),
            MetaspaceConfigError::InvalidChunkSizes { .. }
        ));
    }

    #[test]
    fn config_zero_chunk_size_rejected() {
        let cfg = MetaspaceConfig {
            small_chunk_size: 0,
            ..MetaspaceConfig::default()
        };
        assert!(matches!(
            Metaspace::new(cfg).unwrap_err(),
            MetaspaceConfigError::InvalidChunkSizes { .. }
        ));
    }

    // -- Active-chunk fast path (perf) --------------------------------------

    #[test]
    fn fast_path_bumps_same_chunk() {
        // Repeated small allocations for one loader must keep landing in the
        // same chunk at sequential offsets — identical to the linear scan.
        let mut ms = Metaspace::with_default();
        let a1 = ms.allocate(64, 1).unwrap();
        let a2 = ms.allocate(64, 1).unwrap();
        let a3 = ms.allocate(64, 1).unwrap();
        assert_eq!(a1.chunk_id, a2.chunk_id);
        assert_eq!(a2.chunk_id, a3.chunk_id);
        assert_eq!(a1.offset, 0);
        assert_eq!(a2.offset, 64);
        assert_eq!(a3.offset, 128);
        assert_eq!(ms.chunks.len(), 1);
        assert_eq!(ms.total_used, 192);
    }

    #[test]
    fn fast_path_picks_first_fitting_chunk_not_active() {
        // Loader fills its first (small) chunk, spills into a second, then a
        // tiny allocation that still fits the FIRST chunk must go there —
        // proving the fast path falls back to first-fit, not last-active.
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 256;
        let mut ms = Metaspace::new(cfg).unwrap();
        let a1 = ms.allocate(200, 1).unwrap(); // chunk A, offset 0, 56 left
        let a2 = ms.allocate(200, 1).unwrap(); // A can't fit -> chunk B
        assert_ne!(a1.chunk_id, a2.chunk_id);
        // 32 bytes still fits chunk A's 56-byte tail; must reuse A at offset 200.
        let a3 = ms.allocate(32, 1).unwrap();
        assert_eq!(a3.chunk_id, a1.chunk_id);
        assert_eq!(a3.offset, 200);
        assert_eq!(ms.chunks.len(), 2);
    }

    #[test]
    fn fast_path_invalidated_after_free_and_reuse() {
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 4096;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(1024, 1).unwrap();
        ms.free_loader_metaspace(1);
        // Reusing the freed chunk for a new loader must still work and yield
        // offset 0 with the cache correctly invalidated.
        let a = ms.allocate(512, 2).unwrap();
        assert_eq!(ms.chunks.len(), 1);
        assert_eq!(a.offset, 0);
        assert_eq!(ms.chunks[0].owner_loader_id, 2);
        // Subsequent bump for loader 2 lands in the same reused chunk.
        let b = ms.allocate(256, 2).unwrap();
        assert_eq!(b.chunk_id, a.chunk_id);
        assert_eq!(b.offset, 512);
    }

    #[test]
    fn fast_path_survives_interleaved_loaders() {
        // Interleaving two loaders must not cross-contaminate the per-loader
        // active-chunk cache.
        let mut ms = Metaspace::with_default();
        let a1 = ms.allocate(64, 1).unwrap();
        let b1 = ms.allocate(64, 2).unwrap();
        let a2 = ms.allocate(64, 1).unwrap();
        let b2 = ms.allocate(64, 2).unwrap();
        assert_eq!(a1.chunk_id, a2.chunk_id);
        assert_eq!(b1.chunk_id, b2.chunk_id);
        assert_ne!(a1.chunk_id, b1.chunk_id);
        assert_eq!(a2.offset, 64);
        assert_eq!(b2.offset, 64);
    }

    #[test]
    fn fast_path_invalidated_after_gc() {
        // After a GC reshuffles chunk indices, allocation must still be exact.
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 256;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(64, 1).unwrap();
        ms.allocate(64, 2).unwrap();
        ms.free_loader_metaspace(1);
        ms.trigger_gc(); // removes loader 1's freed chunk, shifts indices
                         // Loader 2's chunk survived; further allocation bumps it correctly.
        let a = ms.allocate(64, 2).unwrap();
        assert_eq!(a.offset, 64);
        assert_eq!(ms.chunks.len(), 1);
        assert_eq!(ms.chunks[0].owner_loader_id, 2);
    }

    // ======================================================================
    // BOUNDED-METADATA FIX (2026-07-26): unload must RELEASE registry rows.
    // `mark_dead` alone left one `ClassLoaderMetaspace` (owned `String`
    // included) per loader ever created, for the process lifetime.
    // ======================================================================

    #[test]
    fn registry_prune_dead_releases_rows() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(1, "A".into());
        reg.register_loader(2, "B".into());
        reg.register_loader(3, "C".into());
        reg.mark_dead(1);
        reg.mark_dead(3);
        // Reporting pass still sees them...
        assert_eq!(reg.dead_loaders(), 2);
        assert_eq!(reg.total_loaders(), 3);
        // ...then they are released.
        assert_eq!(reg.prune_dead(), 2);
        assert_eq!(reg.total_loaders(), 1);
        assert_eq!(reg.dead_loaders(), 0);
        assert!(reg.get_loader_stats(2).is_some());
        assert!(reg.get_loader_stats(1).is_none());
        assert!(reg.get_loader_stats(3).is_none());
    }

    #[test]
    fn registry_remove_loader_returns_row() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(7, "Plugin".into());
        reg.record_allocation(7, 512);
        reg.record_class_loaded(7);
        let removed = reg
            .remove_loader(7)
            .expect("metaspace: expected a row for loader 7");
        assert_eq!(removed.loader_name, "Plugin");
        assert_eq!(removed.allocated_bytes, 512);
        assert_eq!(removed.class_count, 1);
        assert_eq!(reg.total_loaders(), 0);
        assert!(reg.remove_loader(7).is_none());
    }

    // The registry must reach a steady state under repeated define/unload
    // cycles, not grow one row per cycle.
    #[test]
    fn registry_bounded_across_repeated_unload_cycles() {
        let mut reg = MetaspaceRegistry::new();
        reg.register_loader(0, "bootstrap".into());
        for cycle in 0..128u64 {
            let id = 1000 + cycle;
            reg.register_loader(id, "ProxyLoader".into());
            reg.record_allocation(id, 4096);
            reg.record_class_loaded(id);
            reg.mark_dead(id);
            assert_eq!(reg.prune_dead(), 1, "cycle {cycle}");
        }
        assert_eq!(
            reg.total_loaders(),
            1,
            "registry grew across unload cycles — the unbounded-metadata leak is back"
        );
        assert_eq!(reg.dead_loaders(), 0);
    }

    // Freeing a loader's chunks and then running a metaspace GC must return
    // the capacity, so `Metaspace` itself shrinks on unload.
    #[test]
    fn metaspace_capacity_shrinks_on_loader_unload() {
        let mut cfg = MetaspaceConfig::default();
        cfg.small_chunk_size = 256;
        let mut ms = Metaspace::new(cfg).unwrap();
        ms.allocate(64, 1).unwrap();
        ms.allocate(64, 2).unwrap();
        let capacity_before = ms.total_capacity;
        assert!(capacity_before > 0);

        let freed = ms.free_loader_metaspace(1);
        assert_eq!(freed.chunks_freed, 1);
        assert_eq!(freed.loader_id, 1);
        // Freeing alone only flags the chunk; capacity is returned by the GC.
        assert_eq!(ms.total_capacity, capacity_before);

        let gc = ms.trigger_gc();
        assert!(gc.bytes_reclaimed > 0);
        assert!(
            ms.total_capacity < capacity_before,
            "metaspace capacity must shrink once an unloaded loader's chunks are collected"
        );
        assert_eq!(ms.chunks.len(), 1);
        assert_eq!(ms.chunks[0].owner_loader_id, 2);
    }
}
