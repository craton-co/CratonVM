// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! g1ep7: root regions (HotSpot's `G1CMRootMemRegions`).
//!
//! A concurrent cycle opens right after a young-only pause, so at mark start
//! the young generation is that pause's survivors plus what the mutators
//! allocated since. HotSpot never marks or traces a young object: young
//! regions have TAMS at their bottom (implicitly live), and the concurrent
//! start's survivors are scanned once, as root regions, so every object they
//! name is marked. Every later young object is either such a survivor's copy
//! or allocated after the snapshot, so it carries no marking obligation of its
//! own, and an SATB entry naming it can be dropped.
//!
//! This collector used to mark and trace young objects below TAMS like Old
//! ones. Each young pause of the cycle then had to carry them: evacuate the
//! gray and SATB young entries as keep-alive roots, re-gray their copies, and
//! remap the whole gray set (`G1epKeepRounds 10 -Xmx1g`: about 0.6 s of
//! pauses). In a root-region cycle ([`G1Collector::root_regions_cycle`]):
//!
//! * the initial mark walks every young region below its snapshot cursor and
//!   scans each object's references as the marker scans a gray object
//!   ([`G1Collector::scan_root_region_objects`]), inside the stop-the-world
//!   initial mark, on the evacuation pool, so nothing can move or overwrite them
//!   first;
//! * young regions get TAMS 0, so every liveness verdict
//!   (`live_against_mark_snapshot`, cleanup, the scrub) reads them as live;
//! * the marker never pushes or scans a young address
//!   ([`G1Collector::root_region_skips`]), and the SATB drains drop young
//!   entries.
//!
//! A cycle declines (and marks young objects as before) when a young region
//! does not parse below its cursor, or a frozen peer's reserved TLAB tail lies
//! in one: objects inside a tail cannot be walked, and the old marker traces
//! them when it reaches them. The `System.gc()` full cycle never uses it: its
//! remark judges young referents by the bitmap.
//!
//! `CRATONVM_G1_ROOT_REGIONS` (default ON, `=0` kills).

use super::*;

/// Cycles that ran with root regions, cycles that declined them, and the
/// objects / bytes / microseconds the initial marks' scans took (census).
static ROOT_REGION_CYCLES: AtomicU64 = AtomicU64::new(0);
static ROOT_REGION_DECLINED: AtomicU64 = AtomicU64::new(0);
static ROOT_REGION_OBJECTS: AtomicU64 = AtomicU64::new(0);
static ROOT_REGION_BYTES: AtomicU64 = AtomicU64::new(0);
static ROOT_REGION_SCAN_US: AtomicU64 = AtomicU64::new(0);

/// `[cycles, declined, objects, bytes, scan_us]` for the census line.
pub fn g1_root_region_census() -> [u64; 5] {
    [
        ROOT_REGION_CYCLES.load(Ordering::Relaxed),
        ROOT_REGION_DECLINED.load(Ordering::Relaxed),
        ROOT_REGION_OBJECTS.load(Ordering::Relaxed),
        ROOT_REGION_BYTES.load(Ordering::Relaxed),
        ROOT_REGION_SCAN_US.load(Ordering::Relaxed),
    ]
}

/// Objects per pool job of the root-region scan, at least.
const ROOT_REGION_JOB_OBJECTS: usize = 4096;

/// `CRATONVM_G1_ROOT_REGIONS` (default ON, `=0` kills). Read once per cycle.
pub(super) fn g1_root_regions_enabled() -> bool {
    true
}

#[inline]
fn is_young(ty: RegionType) -> bool {
    matches!(ty, RegionType::Eden | RegionType::Survivor)
}

/// The parse of the young regions at mark start: each object's address and
/// size, or `None` when one region cannot be walked. Pure over the region
/// table (the caller holds it still).
fn parse_young_regions(
    regions: &[G1Region],
    reserved_tails: &[(usize, usize)],
) -> Option<Vec<(usize, usize)>> {
    let mut objs = Vec::new();
    for (i, r) in regions.iter().enumerate() {
        if !is_young(r.region_type()) || r.cursor() == 0 {
            continue;
        }
        let base = r.data.addr();
        let end = base + r.cursor();
        if reserved_tails.iter().any(|&(s, e)| s < end && e > base) {
            return None;
        }
        let parsed = compact_in_place::walk_region(regions, i, &[], |p, h, size| {
            if !is_tlab_filler_header(h) {
                objs.push((p as usize, size));
            }
        });
        if !parsed {
            return None;
        }
    }
    Some(objs)
}

impl G1Collector {
    /// Is `region` young in a root-region cycle, i.e. are its objects
    /// implicitly live and never traced?
    #[inline]
    pub(super) fn root_region_skips(&self, region: &G1Region) -> bool {
        self.root_regions_cycle.load(Ordering::Relaxed) && is_young(region.region_type())
    }

    /// At mark start, under the regions write guard and after every region's
    /// TAMS row is written: parse the young regions and, when they all parse,
    /// put their TAMS at 0 and answer their objects. `None` declines (nothing
    /// changed).
    pub(super) fn root_regions_prepare(
        &self,
        regions: &mut [G1Region],
        reserved_tails: &[(usize, usize)],
    ) -> Option<Vec<(usize, usize)>> {
        let Some(objs) = parse_young_regions(regions, reserved_tails) else {
            ROOT_REGION_DECLINED.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        for r in regions.iter_mut() {
            if is_young(r.region_type()) {
                r.mark_start = Some((r.reuse_epoch, 0, r.region_type()));
            }
        }
        ROOT_REGION_CYCLES.fetch_add(1, Ordering::Relaxed);
        Some(objs)
    }

    /// Scan the root-region objects: every reference they hold to a non-young
    /// object that is not marked yet is grayed, exactly as the marker grays a
    /// scanned object's children (the referent skip set, the overlay rows and
    /// the loader side tables included). Stop-the-world, at the end of the
    /// initial mark; [`Self::root_regions_cycle`] is already set, so a young
    /// child is not pushed.
    pub(super) fn scan_root_region_objects(&self, objs: &[(usize, usize)]) {
        let t0 = std::time::Instant::now();
        let regions = self.regions.read();
        let side_tables = gc_flags().g1_mark_side_tables.then(|| {
            MarkSideTables::capture(
                self.rset_cache_epoch.load(Ordering::Acquire),
                self.arena_base..self.arena_end,
            )
        });
        // On the evacuation pool: each job scans a slice of the objects into
        // its own list; the lists reach the gray set afterwards, on the driver.
        let pool = self.evac_pool();
        let jobs = (pool.helpers() + 1).min(objs.len().div_ceil(ROOT_REGION_JOB_OBJECTS)).max(1);
        let per = objs.len().div_ceil(jobs).max(1);
        let lists: Vec<Mutex<Vec<usize>>> = (0..jobs).map(|_| Mutex::new(Vec::new())).collect();
        let regions_len = regions.len();
        let base = RegionsBase(regions.as_ptr() as *mut G1Region);
        let next = AtomicUsize::new(0);
        let body = |_helper: usize| loop {
            let j = next.fetch_add(1, Ordering::Relaxed);
            if j >= jobs {
                break;
            }
            let table: &RegionsBase = &base;
            // SAFETY: the read guard above keeps the table alive and, with
            // every mutator stopped, still; every access here is a shared read.
            let view: &[G1Region] =
                unsafe { std::slice::from_raw_parts(table.0 as *const G1Region, regions_len) };
            let mut local: Vec<usize> = Vec::new();
            let mut out = lists[j].lock();
            for &(addr, _) in objs.iter().skip(j * per).take(per) {
                self.scan_root_region_object(addr, view, &mut local, side_tables.as_ref());
                out.extend(local.drain(..).filter(|&child| gray_entry_addr(child) != addr));
            }
        };
        pool.scope(jobs - 1, &body, || body(0));
        self.push_root_region_children(&regions, lists.into_iter().flat_map(|l| l.into_inner()));
        drop(regions);
        let bytes: usize = objs.iter().map(|&(_, size)| size).sum();
        ROOT_REGION_OBJECTS.fetch_add(objs.len() as u64, Ordering::Relaxed);
        ROOT_REGION_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        ROOT_REGION_SCAN_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// Scan one root-region object's references into `local`. A long
    /// reference array is scanned chunk by chunk; the marker would come back
    /// for each continuation, but it never pops a young address in this cycle,
    /// so every chunk is scanned here (the caller drops the continuation
    /// entries).
    fn scan_root_region_object(
        &self,
        addr: usize,
        view: &[G1Region],
        local: &mut Vec<usize>,
        side_tables: Option<&MarkSideTables>,
    ) {
        // SAFETY: a parsed object start below a young region's cursor, under
        // the regions guard, with every mutator stopped.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let chunks = if header.kind() == ObjectKind::Array
            && header.element_type() == ArrayElementType::Reference
        {
            (header.array_length() as usize)
                .div_ceil(MARK_ARRAY_CHUNK)
                .clamp(1, GRAY_CHUNK_MAX + 1)
        } else {
            1
        };
        for chunk in 0..chunks {
            self.scan_object_refs(addr as *mut u8, header, view, usize::MAX, local, chunk, side_tables);
        }
    }

    /// Gray the scanned children: onto the mark worklist below its cap; at the
    /// cap, black without a scan, and the overflow rescan scans it (the
    /// seed-class cap arm).
    fn push_root_region_children(&self, regions: &[G1Region], children: impl Iterator<Item = usize>) {
        let mut worklist = self.mark_worklist.lock();
        for child in children {
            if worklist.len() < MARK_WORKLIST_CAP {
                worklist.push(child);
                continue;
            }
            if let Some(idx) = self.lookup_region_for_addr(child) {
                if self.cap_arm_seed_passes_gray_gate(&regions[idx], child) {
                    regions[idx].try_mark_and_account(child);
                }
            }
            self.mark_worklist_overflowed.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests;
