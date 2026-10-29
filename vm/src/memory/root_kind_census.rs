// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 1 L7 -- `CRATONVM_DBG=root-kind-census`
//! (`CRATONVM_DBG_ROOT_KIND_CENSUS`, diagnostic, default OFF, read-only):
//! which ROOT KIND keeps how many heap bytes reachable, on every collector.
//!
//! # Why
//!
//! `docs/feature-designs/jit-ir-optimizations.md`: on a
//! failing G1 `-Xmx64m` run the ladder's last census read `old=62.4 MB` where
//! the probe's design (and HotSpot, which never throws) keeps about 45 MB. The
//! page asked "what the extra ~17 MB is". The `old=` figure is the Old
//! regions' USED extent (`G1RegionCensus::old`, the sum of region cursors),
//! not the marked live set, so it cannot tell RETENTION (a root that keeps
//! garbage reachable) from FRAGMENTATION (garbage the copying compaction
//! cannot take because the Free pool is too small to evacuate into). This
//! census answers both questions from one pause:
//!
//! * per root KIND -- every `collect_roots` step (the scan marks), every
//!   thread's registry snapshot (split per thread, with its blocked flag and
//!   whether its deposit deferred a compiled chain), the frozen in-JIT peers'
//!   take-over and helper-window words (the lazy blocked deposit's resolved
//!   chains are among them), then the pause's extra kinds (finalizable
//!   candidates, published JIT pin words) -- the objects and bytes it reaches
//!   FIRST (kinds in root-vector order), its top classes, and, for the kinds
//!   that hold at least 1% of the total, the bytes ONLY it keeps (`exclusive`:
//!   total minus the reach of every other kind);
//! * the ten single roots that reach the most bytes first;
//! * on G1 with a walkable heap, the regions: used bytes walked, bytes the
//!   roots reach, regions the roots reach NOTHING in (a trusted mark would free
//!   them whole), and the reached/used deciles of the rest (fragmentation).
//!
//! Read the result against HotSpot's `jmap -histo:live` at the same point: a
//! kind whose exclusive bytes HotSpot does not have is the retention; a
//! reached total near HotSpot's live set with a large used figure is
//! fragmentation (the copying compaction's room, G1-owner work).
//!
//! # When
//!
//! `=1` (or `CRATONVM_DBG=root-kind-census`): the allocation-failure,
//! humongous, G1 compaction and explicit (`System.gc()`, jcmd, JVMTI, dump)
//! collection pauses, the G1 stop-the-world full mark and the G1 remark.
//! `=all`: every collection pause too. Off: one flag read per pause.
//!
//! # Cost
//!
//! One traversal of the reachable heap per census pause, plus one traversal of
//! the cached graph per exclusive kind (at most [`MAX_EXCLUSIVE_KINDS`]), plus
//! one linear heap walk on G1. All inside the pause, all diagnostics.

use cratonvm_types::ObjectRef;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::vm::SharedVm;

/// The switch's key.
const KEY: &str = "CRATONVM_DBG_ROOT_KIND_CENSUS";

/// At most this many kinds get an exclusive figure per census.
pub(crate) const MAX_EXCLUSIVE_KINDS: usize = 12;

/// A kind gets an exclusive figure only when it reached at least this share
/// of the total first (an object only kind K reaches is reached first by K,
/// so a kind's exclusive bytes never exceed its first-reached bytes).
const EXCLUSIVE_MIN_SHARE: f64 = 0.01;

/// When does the census run? Read per pause (no process static: the per-VM
/// statics ratchet counts declarations).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CensusMode {
    Off,
    /// The failure, explicit and G1 mark-cycle pauses only.
    Selected,
    /// Every collection pause too.
    All,
}

/// The mode from `CRATONVM_DBG_ROOT_KIND_CENSUS`: unset or an off word is
/// [`CensusMode::Off`], `all` is [`CensusMode::All`], any other on word is
/// [`CensusMode::Selected`].
pub(crate) fn mode() -> CensusMode {
    if !cratonvm_types::flags::runtime_flag_on(KEY) {
        return CensusMode::Off;
    }
    let all = cratonvm_types::flags::runtime_var_os(KEY)
        .is_some_and(|v| v.to_string_lossy().trim().eq_ignore_ascii_case("all"));
    if all {
        CensusMode::All
    } else {
        CensusMode::Selected
    }
}

/// Is the census on at all? (The `collect_roots` scan marks follow it.)
pub(crate) fn enabled() -> bool {
    mode() != CensusMode::Off
}

/// One root kind: its label and its root words, in root-vector order.
pub(crate) struct Kind {
    pub label: String,
    pub roots: Vec<usize>,
}

/// One kind's line of the census.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct KindRow {
    pub label: String,
    /// Root entries of the kind (non-zero words).
    pub roots: usize,
    /// Of those, the ones that start an object.
    pub resolved: usize,
    /// Objects and bytes this kind reached before any later kind.
    pub first_objects: u64,
    pub first_bytes: u64,
    /// Bytes no other kind reaches; `None` when not computed.
    pub exclusive_bytes: Option<u64>,
    /// `(class id, objects, bytes)` of the first-reached objects, largest
    /// bytes first, at most five.
    pub top_classes: Vec<(u32, u64, u64)>,
}

/// One single root's first-reached bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TopRoot {
    pub kind: usize,
    pub index_in_kind: usize,
    pub addr: usize,
    pub class_id: u32,
    pub objects: u64,
    pub bytes: u64,
}

/// What [`attribute`] answers.
#[derive(Debug, Default)]
pub(crate) struct CensusResult {
    pub rows: Vec<KindRow>,
    pub top_roots: Vec<TopRoot>,
    pub total_objects: u64,
    pub total_bytes: u64,
    /// Every reached object: start -> (size, class id).
    // addr-keyed: one pause's reached objects, local to one census.
    pub reached: FxHashMap<usize, (usize, u32)>,
}

/// How many single roots [`attribute`] reports.
const TOP_ROOTS: usize = 10;

/// The graph [`attribute`] traverses, cached from its first pass: per reached
/// object its size, class and children; `None` for a word that is no object.
#[derive(Default)]
struct Graph {
    // addr-keyed: one census's object graph, local to one call.
    nodes: FxHashMap<usize, Option<(usize, u32, u32, u32)>>,
    kids: Vec<usize>,
}

impl Graph {
    /// Learn `addr` through `obj` (once) and answer its node.
    fn learn(
        &mut self,
        addr: usize,
        obj: &mut dyn FnMut(usize, &mut dyn FnMut(usize)) -> Option<(usize, u32)>,
    ) -> Option<(usize, u32, u32, u32)> {
        if let Some(n) = self.nodes.get(&addr) {
            return *n;
        }
        let start = self.kids.len();
        let kids = &mut self.kids;
        let info = obj(addr, &mut |c| {
            if c != 0 {
                kids.push(c);
            }
        });
        let node = match info {
            Some((size, cid)) => Some((
                size,
                cid,
                u32::try_from(start).unwrap_or(u32::MAX),
                u32::try_from(self.kids.len() - start).unwrap_or(0),
            )),
            None => {
                self.kids.truncate(start);
                None
            }
        };
        self.nodes.insert(addr, node);
        node
    }

    fn children(&self, node: (usize, u32, u32, u32)) -> &[usize] {
        let start = node.2 as usize;
        let len = node.3 as usize;
        self.kids.get(start..start + len).unwrap_or(&[])
    }
}

/// The attribution. `obj(addr, visit)` answers the object starting at `addr`
/// (`(size, class id)`) and passes each reference it holds to `visit`, or
/// `None` for a word that starts no object. Kinds are credited in order: an
/// object goes to the first kind (and the first root of it) whose traversal
/// reaches it. Then, for the kinds holding at least
/// [`EXCLUSIVE_MIN_SHARE`] of the total, at most `max_exclusive` of them
/// (largest first), the bytes no other kind reaches. Pure over `obj`.
pub(crate) fn attribute(
    kinds: &[Kind],
    obj: &mut dyn FnMut(usize, &mut dyn FnMut(usize)) -> Option<(usize, u32)>,
    max_exclusive: usize,
) -> CensusResult {
    let mut graph = Graph::default();
    let mut result = CensusResult::default();
    // addr-keyed: this census's reached objects, local to one call.
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    let mut stack: Vec<usize> = Vec::new();
    for (k, kind) in kinds.iter().enumerate() {
        let mut row = KindRow {
            label: kind.label.clone(),
            ..KindRow::default()
        };
        let mut classes: FxHashMap<u32, (u64, u64)> = FxHashMap::default();
        for (j, &root) in kind.roots.iter().enumerate() {
            if root == 0 {
                continue;
            }
            row.roots += 1;
            let Some(root_node) = graph.learn(root, obj) else {
                continue;
            };
            row.resolved += 1;
            let (mut objects, mut bytes) = (0u64, 0u64);
            stack.clear();
            stack.push(root);
            while let Some(a) = stack.pop() {
                if seen.contains(&a) {
                    continue;
                }
                let Some(node) = graph.learn(a, obj) else {
                    continue;
                };
                seen.insert(a);
                objects += 1;
                bytes += node.0 as u64;
                let c = classes.entry(node.1).or_default();
                c.0 += 1;
                c.1 += node.0 as u64;
                result.reached.insert(a, (node.0, node.1));
                for &child in graph.children(node) {
                    if !seen.contains(&child) {
                        stack.push(child);
                    }
                }
            }
            row.first_objects += objects;
            row.first_bytes += bytes;
            if bytes > 0 {
                note_top_root(
                    &mut result.top_roots,
                    TopRoot {
                        kind: k,
                        index_in_kind: j,
                        addr: root,
                        class_id: root_node.1,
                        objects,
                        bytes,
                    },
                );
            }
        }
        let mut top: Vec<(u32, u64, u64)> =
            classes.into_iter().map(|(c, (n, b))| (c, n, b)).collect();
        top.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        top.truncate(5);
        row.top_classes = top;
        result.total_objects += row.first_objects;
        result.total_bytes += row.first_bytes;
        result.rows.push(row);
    }
    // Exclusive figures, on the cached graph only (no heap read).
    let floor = (result.total_bytes as f64 * EXCLUSIVE_MIN_SHARE) as u64;
    let mut order: Vec<usize> = (0..result.rows.len())
        .filter(|&k| result.rows[k].first_bytes > 0 && result.rows[k].first_bytes >= floor)
        .collect();
    order.sort_by(|&a, &b| {
        result.rows[b]
            .first_bytes
            .cmp(&result.rows[a].first_bytes)
            .then(a.cmp(&b))
    });
    order.truncate(max_exclusive);
    for k in order {
        // addr-keyed: one exclusive pass's reached objects, local to it.
        let mut others: FxHashSet<usize> = FxHashSet::default();
        let mut bytes = 0u64;
        for (i, kind) in kinds.iter().enumerate() {
            if i == k {
                continue;
            }
            for &root in &kind.roots {
                stack.clear();
                stack.push(root);
                while let Some(a) = stack.pop() {
                    if others.contains(&a) {
                        continue;
                    }
                    let Some(Some(node)) = graph.nodes.get(&a).copied() else {
                        continue;
                    };
                    others.insert(a);
                    bytes += node.0 as u64;
                    for &child in graph.children(node) {
                        if !others.contains(&child) {
                            stack.push(child);
                        }
                    }
                }
            }
        }
        result.rows[k].exclusive_bytes = Some(result.total_bytes.saturating_sub(bytes));
    }
    result
}

/// Keep the [`TOP_ROOTS`] largest single roots, largest first.
fn note_top_root(top: &mut Vec<TopRoot>, r: TopRoot) {
    if top.len() == TOP_ROOTS && top.last().is_some_and(|l| l.bytes >= r.bytes) {
        return;
    }
    let at = top.partition_point(|t| t.bytes >= r.bytes);
    top.insert(at, r);
    top.truncate(TOP_ROOTS);
}

/// Per-region fragmentation summary: `(region index -> (used walked, reached))`
/// folded into `(regions, used, reached, regions reached nothing in, deciles)`
/// where `deciles[d]` counts the regions with reached/used in
/// `[d/10, (d+1)/10)` (the last bucket includes 1.0). Pure.
pub(crate) fn region_summary(
    // addr-keyed: not an address (a region index), local to one census.
    per_region: &FxHashMap<usize, (u64, u64)>,
) -> (usize, u64, u64, usize, [usize; 10]) {
    let mut used = 0u64;
    let mut reached = 0u64;
    let mut zero = 0usize;
    let mut deciles = [0usize; 10];
    for &(u, r) in per_region.values() {
        used += u;
        reached += r;
        if r == 0 {
            zero += 1;
        }
        let d = if u == 0 {
            0
        } else {
            ((r.min(u) * 10) / u).min(9) as usize
        };
        deciles[d] += 1;
    }
    (per_region.len(), used, reached, zero, deciles)
}

/// The census's name for a class id (`try_read`: a busy table prints the id).
fn class_name(shared: &SharedVm, cid: u32) -> String {
    shared
        .classes
        .class_manager
        .try_read()
        .and_then(|cm| {
            cm.get_class(crate::classloading::ClassId::new(cid))
                .map(|c| c.name.to_string())
        })
        .unwrap_or_else(|| format!("class_id={cid}"))
}

/// Each registered thread's snapshot words, labelled `snapshot "<name>"
/// tid=N blocked=B deferred_chain=D` (`try_lock`s: a busy snapshot is left
/// unlabelled). First thread wins an address.
// addr-keyed: one pause's snapshot words -> their thread's label, local to one census.
fn snapshot_labels(shared: &SharedVm) -> FxHashMap<usize, String> {
    let registry = &shared.threads.thread_registry;
    // addr-keyed: as above.
    let mut out: FxHashMap<usize, String> = FxHashMap::default();
    for (tid, name) in registry.all_thread_names() {
        let Some(arcs) = registry.starting_thread_shared(tid) else {
            continue;
        };
        let Some(words) = arcs
            .root_snapshot
            .try_lock()
            .map(|s| s.iter().map(|o| o.as_ptr() as usize).collect::<Vec<usize>>())
        else {
            continue;
        };
        let block = &arcs.gc_block_state;
        let blocked = block
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        let deferred = block
            .deferred_jit_chain
            .try_lock()
            .map(|r| if r.is_some() { "true" } else { "false" })
            .unwrap_or("?");
        let label = format!(
            "snapshot {name:?} tid={} blocked={blocked} deferred_chain={deferred}",
            tid.0
        );
        for w in words {
            out.entry(w).or_insert_with(|| label.clone());
        }
    }
    out
}

/// Split the root vector into kinds: `sections` (start index, label) name
/// its parts in order; inside `snapshot_range` each word takes its thread's
/// snapshot label instead (a word no snapshot names keeps the section's).
/// Then the `extra` kinds. Pure over `snapshot_label`.
pub(crate) fn split_kinds(
    roots: &[usize],
    sections: &[(usize, &'static str)],
    snapshot_range: Option<(usize, usize)>,
    snapshot_label: &dyn Fn(usize) -> Option<String>,
    extra: Vec<(String, Vec<usize>)>,
) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = Vec::new();
    let mut index_of: FxHashMap<String, usize> = FxHashMap::default();
    let mut push = |label: String, word: usize, kinds: &mut Vec<Kind>| {
        let k = *index_of.entry(label.clone()).or_insert_with(|| {
            kinds.push(Kind {
                label,
                roots: Vec::new(),
            });
            kinds.len() - 1
        });
        kinds[k].roots.push(word);
    };
    for (i, &w) in roots.iter().enumerate() {
        let section = sections
            .iter()
            .take_while(|&&(start, _)| start <= i)
            .last()
            .map_or("<before-first-section>", |&(_, l)| l);
        let label = match snapshot_range {
            Some((from, to)) if i >= from && i < to => {
                snapshot_label(w).unwrap_or_else(|| section.to_string())
            }
            _ => section.to_string(),
        };
        push(label, w, &mut kinds);
    }
    for (label, words) in extra {
        for w in words {
            push(label.clone(), w, &mut kinds);
        }
    }
    kinds
}

/// Run the census for one pause and print it (stderr, `[root-census]`).
/// `roots` is the vector the collector marks from, `sections` its parts,
/// `snapshot_range` the registry snapshots' part, `extra` the pause's other
/// kinds. `walkable`: a linear heap walk is safe (no unretired TLAB tail);
/// only then, and only on G1, the region summary runs. Must run inside the
/// pause, before the collection moves anything.
pub(crate) fn run(
    shared: &SharedVm,
    site: &str,
    roots: &[ObjectRef],
    sections: &[(usize, &'static str)],
    snapshot_range: Option<(usize, usize)>,
    extra: Vec<(String, Vec<usize>)>,
    walkable: bool,
) {
    let heap = &shared.mem.heap;
    let words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    let labels = if snapshot_range.is_some() {
        snapshot_labels(shared)
    } else {
        FxHashMap::default()
    };
    let kinds = split_kinds(
        &words,
        sections,
        snapshot_range,
        &|w| labels.get(&w).cloned(),
        extra,
    );
    let started = std::time::Instant::now();
    let result = attribute(
        &kinds,
        &mut |addr, visit| heap.dbg_object_size_and_refs(addr, visit),
        MAX_EXCLUSIVE_KINDS,
    );
    let pause = heap.collection_count();
    eprintln!(
        "[root-census] site={site} pause={pause} kinds={} roots={} reached_objects={} \
         reached_bytes={} census_ms={}",
        kinds.len(),
        words.len(),
        result.total_objects,
        result.total_bytes,
        started.elapsed().as_millis()
    );
    for (k, row) in result.rows.iter().enumerate() {
        if row.roots == 0 {
            continue;
        }
        let top: Vec<String> = row
            .top_classes
            .iter()
            .map(|&(cid, n, b)| format!("{} n={n} bytes={b}", class_name(shared, cid)))
            .collect();
        eprintln!(
            "[root-census]   kind#{k} {:?} roots={} resolved={} first_objects={} first_bytes={} \
             exclusive_bytes={} top=[{}]",
            row.label,
            row.roots,
            row.resolved,
            row.first_objects,
            row.first_bytes,
            row.exclusive_bytes
                .map_or_else(|| "-".to_string(), |b| b.to_string()),
            top.join("; ")
        );
    }
    for t in &result.top_roots {
        eprintln!(
            "[root-census]   top-root kind#{} idx={} addr=0x{:x} class={} objects={} bytes={}",
            t.kind,
            t.index_in_kind,
            t.addr,
            class_name(shared, t.class_id),
            t.objects,
            t.bytes
        );
    }
    if let crate::memory::vm_heap::VmHeap::G1(h) = heap {
        let c = h.region_census();
        eprintln!(
            "[root-census]   g1-census free={} eden={} survivor={} old={}/{} humongous={}/{} \
             region_size={}",
            c.free,
            c.eden.0,
            c.survivor.0,
            c.old.0,
            c.old.1,
            c.humongous.0,
            c.humongous.1,
            c.region_size
        );
        // The regions the published JIT pin words keep out of every
        // collection set (and so out of the full compaction's candidates).
        // addr-keyed: not an address (a region index), local to one census.
        let mut pinned_regions: FxHashSet<usize> = FxHashSet::default();
        let pins = cratonvm_gc::gc_quiescence::pinned_jit_roots_snapshot();
        if let Some((base, end)) = heap.conservative_addr_span() {
            if c.region_size > 0 {
                for &p in &pins {
                    if p >= base && p < end {
                        pinned_regions.insert((p - base) / c.region_size);
                    }
                }
            }
        }
        eprintln!(
            "[root-census]   g1-jit-pins words={} regions={}",
            pins.len(),
            pinned_regions.len()
        );
        if walkable && c.region_size > 0 {
            if let Some((base, _end)) = heap.conservative_addr_span() {
                // addr-keyed: not an address (region index -> (used walked, reached)), local to one census.
                let mut per_region: FxHashMap<usize, (u64, u64)> = FxHashMap::default();
                heap.for_each_object(&mut |ptr, size| {
                    let a = ptr as usize;
                    let Some(off) = a.checked_sub(base) else {
                        return;
                    };
                    let slot = per_region.entry(off / c.region_size).or_default();
                    slot.0 += size as u64;
                    if result.reached.contains_key(&a) {
                        slot.1 += size as u64;
                    }
                });
                let (regions, used, reached, zero, deciles) = region_summary(&per_region);
                let (pinned_used, pinned_reached) = pinned_regions
                    .iter()
                    .filter_map(|r| per_region.get(r))
                    .fold((0u64, 0u64), |(u, r), &(pu, pr)| (u + pu, r + pr));
                eprintln!(
                    "[root-census]   g1-regions walked={regions} used={used} reached={reached} \
                     reached_nothing={zero} reached_over_used_deciles={deciles:?} \
                     jit_pinned_used={pinned_used} jit_pinned_reached={pinned_reached}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy heap: `addr -> (size, class, children)`.
    fn toy<'a>(
        objects: &'a [(usize, usize, u32, &'a [usize])],
    ) -> impl FnMut(usize, &mut dyn FnMut(usize)) -> Option<(usize, u32)> + 'a {
        move |addr, visit| {
            let &(_, size, cid, kids) = objects.iter().find(|o| o.0 == addr)?;
            for &k in kids {
                visit(k);
            }
            Some((size, cid))
        }
    }

    fn kind(label: &str, roots: &[usize]) -> Kind {
        Kind {
            label: label.to_string(),
            roots: roots.to_vec(),
        }
    }

    /// First reach wins, in kind order; a shared subgraph is the first kind's;
    /// a non-object word counts as a root but resolves nothing; cycles end.
    #[test]
    fn the_first_kind_to_reach_an_object_is_credited() {
        let heap: &[(usize, usize, u32, &[usize])] = &[
            (0x100, 16, 1, &[0x200, 0x300]),
            (0x200, 1000, 2, &[0x100]),
            (0x300, 50, 3, &[]),
            (0x400, 24, 1, &[0x300, 0x500]),
            (0x500, 2000, 2, &[]),
        ];
        let kinds = vec![
            kind("a", &[0x100, 0x999]),
            kind("b", &[0x400]),
            kind("c", &[0x300]),
        ];
        let mut obj = toy(heap);
        let r = attribute(&kinds, &mut obj, 12);
        assert_eq!(r.total_bytes, 16 + 1000 + 50 + 24 + 2000);
        assert_eq!(r.total_objects, 5);
        assert_eq!((r.rows[0].roots, r.rows[0].resolved), (2, 1));
        assert_eq!(r.rows[0].first_bytes, 1066);
        assert_eq!(r.rows[1].first_bytes, 2024);
        assert_eq!(r.rows[2].first_bytes, 0);
        // Exclusive: a keeps 0x100 and 0x200 alone; 0x300 is shared with b and c.
        assert_eq!(r.rows[0].exclusive_bytes, Some(1016));
        assert_eq!(r.rows[1].exclusive_bytes, Some(2024));
        // c reached nothing first, so it gets no exclusive figure.
        assert_eq!(r.rows[2].exclusive_bytes, None);
        // Top classes by bytes, largest first.
        assert_eq!(r.rows[0].top_classes[0], (2, 1, 1000));
        // Top roots, largest first.
        assert_eq!(r.top_roots[0].addr, 0x400);
        assert_eq!(r.top_roots[0].bytes, 2024);
        assert_eq!(r.top_roots[1].addr, 0x100);
        assert!(r.reached.contains_key(&0x500));
    }

    #[test]
    fn exclusive_is_never_above_first_reach_and_respects_the_cap() {
        let heap: &[(usize, usize, u32, &[usize])] = &[
            (0x10, 100, 1, &[0x20]),
            (0x20, 100, 1, &[]),
            (0x30, 100, 1, &[0x20]),
        ];
        let kinds = vec![kind("x", &[0x10]), kind("y", &[0x30])];
        let mut obj = toy(heap);
        let r = attribute(&kinds, &mut obj, 1);
        // Only the largest kind is computed under a cap of one.
        assert_eq!(r.rows[0].exclusive_bytes, Some(100));
        assert_eq!(r.rows[1].exclusive_bytes, None);
        for row in &r.rows {
            if let Some(e) = row.exclusive_bytes {
                assert!(e <= row.first_bytes);
            }
        }
    }

    #[test]
    fn the_root_vector_splits_by_section_and_by_snapshot_thread() {
        let roots = [1, 2, 3, 4, 5, 6];
        let sections: &[(usize, &'static str)] = &[(0, "s0"), (2, "snaps"), (5, "xt")];
        let label = |w: usize| (w == 3).then(|| "snapshot T1".to_string());
        let kinds = split_kinds(
            &roots,
            sections,
            Some((2, 5)),
            &label,
            vec![("extra".to_string(), vec![9])],
        );
        let names: Vec<&str> = kinds.iter().map(|k| k.label.as_str()).collect();
        // Kinds appear in the order their first word does.
        assert_eq!(names, vec!["s0", "snapshot T1", "snaps", "xt", "extra"]);
        assert_eq!(kinds[0].roots, vec![1, 2]);
        assert_eq!(kinds[1].roots, vec![3]);
        assert_eq!(kinds[2].roots, vec![4, 5]);
        assert_eq!(kinds[3].roots, vec![6]);
        assert_eq!(kinds[4].roots, vec![9]);
    }

    #[test]
    fn the_region_summary_counts_unreached_regions_and_deciles() {
        // addr-keyed: not an address (a region index), test-local.
        let mut m: FxHashMap<usize, (u64, u64)> = FxHashMap::default();
        m.insert(0, (1000, 0));
        m.insert(1, (1000, 1000));
        m.insert(2, (1000, 450));
        let (regions, used, reached, zero, d) = region_summary(&m);
        assert_eq!((regions, used, reached, zero), (3, 3000, 1450, 1));
        assert_eq!(d[0], 1);
        assert_eq!(d[4], 1);
        assert_eq!(d[9], 1);
    }

    #[test]
    fn the_switch_is_off_by_default() {
        if cratonvm_types::flags::runtime_var_os(KEY).is_none() {
            assert_eq!(mode(), CensusMode::Off);
        }
    }
}
