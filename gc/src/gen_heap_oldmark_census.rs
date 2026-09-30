// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r4w5/oomjit5 (2026-09-24) — `CRATONVM_DBG=oldmark-root-census`
//! (`CRATONVM_DBG_OLDMARK_ROOT_CENSUS=1`): which ROOT CATEGORY keeps old-gen
//! data alive through a stop-the-world major.
//!
//! # Why this exists
//!
//! `docs/internal/gc/gengc-r4w4-final-oome-from-compiled-code-leaves-dropped-data-reachable-FIXED-20260928.md`:
//! after an `OutOfMemoryError` thrown from compiled code the program drops its
//! data (`head = null`), a Generational major runs, and the next allocation
//! still fails. Something still references the dropped data through the major.
//! The candidates are a dozen root families (a conservative JIT band word, a
//! precise oop-map slot, a stashed deopt frame, a native pin, a handle slot,
//! a parked snapshot, a young object the major seeds from, ...), and the
//! major's mark sees one flat `roots` slice with no labels. Before this, the
//! only way to name the holder was to switch families off one at a time.
//!
//! # What it does
//!
//! Immediately before `old_gen_gc`'s seed loop, and only under the token, it
//! walks the SAME root slice the loop is about to seed, in the same order, and
//! runs its own READ-ONLY breadth-first traversal (its own visited set; it
//! never touches a mark bit, so the real mark is unaffected):
//!
//! 1. every root entry, in slice order, labelled by the most specific registry
//!    that names its address (see [`label_for`]);
//! 2. then every young from-space object nothing above reached — the major
//!    seeds old gen from ALL of young from-space (`mark_young_to_old_refs`),
//!    live or not, so what only a young object reaches is attributed to
//!    `young/from-space-unrooted`.
//!
//! The traversal follows heap reference slots of young AND old objects (so a
//! root that holds a young node whose chain continues into old gen is credited
//! with the old bytes behind it) and the external-overlay edges of every object
//! it visits. An old object is credited to the category whose traversal
//! reached it FIRST; the report prints, per category, how many roots it
//! seeded, how many resolved to a heap object, and the old-gen objects and
//! bytes (and young bytes) it reached first, plus the eight single roots that
//! reached the most old bytes.
//!
//! It does NOT follow the loader / mirror / metadata pin side tables, nor the
//! walk-gap seeds and the `close_live_set` repair the sweep applies; what those
//! retain shows up as the difference between `old_live_after` and the census
//! total on the summary line.
//!
//! # Where the labels come from
//!
//! **gen r4w6/oomjit6 (2026-09-24): by INDEX first.** The VM's collection door
//! (`vm/src/runtime/interpreter/gc_and_alloc.rs`, `run_collection_pause`)
//! stages the index sections of the root vector it built
//! ([`stage_root_sections`]): its own `collect_roots` (split per step when
//! `memory::roots::scan_section_of` has marks), the registry's snapshots, and
//! the forcibly-stopped peers' conservative scan. A root inside a staged
//! section is labelled `<section>/<refinement>`, the refinement being the
//! first ADDRESS label below that names it, or `-`. The wave-5 verdict
//! `vm/unattributed` — every root the address registries did not know,
//! i.e. interpreter frames, statics, snapshots — can then no longer occur on
//! a door-staged collection. The VM labeller, published by the same door, now
//! also names the collecting thread's interpreter frame slots (split by WHY
//! they are roots: live, dead-by-liveness, lost-tag), its thread fields, the
//! JNI locals and globals, the shadow stack, the statics, the class mirrors
//! and locks, the string pool and the singleton OOME.
//!
//! Without staging, a label is decided per ADDRESS, in this order:
//!
//! * a VM labeller the VM installs ([`install_vm_labeller`]; the JIT's
//!   exception paths install it — `vm/src/jit/helpers.rs`
//!   `oldmark_census_arm`) — the deopt / exceptional stashes of the collecting
//!   thread, and, while that thread is inside a JIT scope, its pending JIT
//!   throwable, native pins, handle slots, native allocation pool and
//!   native pending return;
//! * `gc_quiescence::root_source_of` — the VM's named native root sources,
//!   when `CRATONVM_DBG_ROOT_SOURCE=1` is also set;
//! * the JIT frame scan's own per-cycle registries: an address a compiled
//!   frame holds in a word NO channel can rewrite
//!   (`jit/band-unrewritable` — the conservative band: callee-saved images,
//!   blind spill, outgoing-argument reserve, spill above the live cursor), an
//!   address a precise oop-map slot holds (`jit/precise-movable`), and any other
//!   address the JIT scans published (`jit/frame-scan`, the union over threads);
//! * the seeding site: the promotion destinations the in-place sweep appends
//!   (`seed/promotion-dest`) and the resurrected finalizables the moving major
//!   appends (`seed/finalizer-old`), via [`TailScope`];
//! * otherwise `vm/unattributed` — interpreter frames, statics, class mirrors,
//!   JNI globals, the string pool, peer snapshots.
//!
//! An address named by several registries takes the first label in that
//! order. For a leak that is what is wanted: the dropped data has no
//! legitimate holder, so whatever registry names it IS the leak.
//!
//! # Cost
//!
//! Nothing without the token: one cached flag load per STW major. With it, one
//! extra traversal of the live heap per major, inside the pause.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    for_each_ref_slot, gen_object_total_size, resync_to_next_free_block, skip_free_blocks,
};
use crate::arena::Arena;
use crate::heap::{array_element_type_from_tag, object_kind_from_tag, ObjectHeader, HEADER_SIZE};
use crate::old_gen::OldGen;
use cratonvm_types::ObjectRef;

/// Is `CRATONVM_DBG=oldmark-root-census` on? Read once.
pub(super) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OLDMARK_ROOT_CENSUS"))
}

/// [`enabled`], for the VM's collection door (re-exported from `gen_heap` as
/// `oldmark_census_enabled`): it stages [`stage_root_sections`] only when the
/// census will read them.
pub fn census_enabled() -> bool {
    enabled()
}

// ---------------------------------------------------------------------------
// gen r4w6/oomjit6 (2026-09-24): index sections staged by the collection door
// ---------------------------------------------------------------------------
//
// Wave 5 labelled a root by its ADDRESS alone: whichever registry happened to
// name that address. An address no registry named -- an interpreter local, a
// static, a peer's snapshot, a frozen peer's register -- printed as
// `vm/unattributed`, and that is the verdict the JIT OOME retention got
// (`docs/internal/reviews/gengc-round4-summary-20260923.md`, wave 5). But the
// door that BUILDS the root vector knows, for every index, which part of the
// VM pushed it: its own thread's `collect_roots` (with per-step marks when
// `roots::scan_section_of` has them), the registry's peer snapshots, the
// forcibly-stopped peers' conservative scan. It stages those index ranges
// here before `collect_garbage`, and the census names every root by the
// section that pushed it, refined by whatever registry names the address.
// Nothing is `unattributed` on a door-staged collection any more; a root the
// refinement cannot place prints as `<section>/-`.

thread_local! {
    /// `(first index, section)` ascending, for the root slice the next
    /// collection on this thread receives. See [`StagedRootSections`].
    static SECTIONS: RefCell<Vec<(usize, &'static str)>> = const { RefCell::new(Vec::new()) };
}

/// RAII scope for [`stage_root_sections`]: restores the previous staging on
/// drop, so a nested collection cannot inherit the outer door's sections.
pub struct StagedRootSections {
    prev: Vec<(usize, &'static str)>,
}

impl Drop for StagedRootSections {
    fn drop(&mut self) {
        let prev = std::mem::take(&mut self.prev);
        let _ = SECTIONS.try_with(|s| *s.borrow_mut() = prev);
    }
}

/// Stage the index sections of the root vector the caller is about to hand
/// `collect_garbage*` on this thread: entry `i` of the vector belongs to the
/// section of the last `(start, name)` with `start <= i`. Unsorted input is
/// sorted (stable, so equal starts keep the caller's order and the LAST one
/// wins). Hold the returned guard across the collection.
pub fn stage_root_sections(mut sections: Vec<(usize, &'static str)>) -> StagedRootSections {
    sections.sort_by_key(|&(start, _)| start);
    let prev = SECTIONS
        .try_with(|s| std::mem::replace(&mut *s.borrow_mut(), sections))
        .unwrap_or_default();
    StagedRootSections { prev }
}

/// The staged section of root `index`, if any.
fn section_of(sections: &[(usize, &'static str)], index: usize) -> Option<&'static str> {
    let i = sections.partition_point(|&(start, _)| start <= index);
    (i > 0).then(|| sections[i - 1].1)
}

// ---------------------------------------------------------------------------
// gen r5w4/jit8 (2026-09-26): the HOLDER census -- every root that keeps a
// marker object alive
// ---------------------------------------------------------------------------
//
// The category table below answers "which root family keeps old bytes alive",
// crediting each object to the FIRST root that reached it. A retention probe
// asks a narrower question: THIS object -- the list a `WeakReference` watches,
// which the program has dropped -- survived `System.gc()`; which roots reach
// it? First-reach cannot answer that: the list can be reachable from several
// roots, and fixing the one named first leaves the others. So the VM's
// collection door stages MARKERS (the referents of the plain
// `java.lang.ref.WeakReference`s active at the pause; see
// `oldmark_census_holder_markers` in `vm/src/jit/helpers.rs`), and for every
// marker the root slice can reach the census prints each HOLDER: the root
// entry (its index, `section/label` and, for a JIT-scan word, the scan's
// provenance), a shortest path from it to the marker, and the other root
// entries that name the same object. After a holder is named its root object
// is BLOCKED and the search is repeated, so each further holder is one the
// previous ones do not explain; the search stops when the marker is
// unreachable or `HOLDER_ROUNDS_MAX` holders are named. An OLD marker the
// roots no longer reach is also tried from young from-space, which the major
// seeds from wholesale (`young/from-space-unrooted` above).
//
// gen r5w6/oomjit10: a major with NO staged marker explains the largest
// root-reached old objects instead (`auto_holder_markers`; their marker line
// says `auto: ...`), so a probe that drops plain arrays and watches nothing
// through a `WeakReference` (`NativeGrowthReclaimProbe`) gets holder lines too.
//
// The referent slot of every active weak/phantom reference is nulled before
// the mark (`weakref_null_referents_pre_gc`), so the watching
// `WeakReference` is never a holder on this path -- if one is reported, the
// pre-GC nulling skipped it, which is a finding of its own.
//
// Reading a holder line:
// * `cat=<section>/<structure>`: the section is WHO pushed the root -- a
//   numbered `collect_roots` step of the collecting thread (the door line
//   names it), `registry snapshots (...)` for a thread's published snapshot
//   (the collecting thread's own included), `xt frozen in-JIT peers (...)`
//   for a forcibly stopped peer; the structure after the last `/` is the VM
//   registry that names the address (`interp-local-dead-by-liveness`,
//   `native-pin`, `native-pending-return`, `deopt-stash`, `static-field`,
//   `jit-band-unrewritable`, `jit-frame-scan`, ...), `-` when none does.
// * `prov="method=... off=... region=..."` is a word of a compiled frame's own
//   band, with its storage class; `prov="scan_one_frame addr=0x..."` is a word
//   a LAYOUT-FREE sweep read (the whole-band fallback, a foreign innermost
//   frame, the A5 above-chain band) -- i.e. a Rust frame or a frame the scan
//   could not describe. The door line's `bandpath=` counters say which of
//   those sweeps ran.
// * `prov=` is keyed by the ADDRESS, not by the root entry (gen r5w5/oomjit9
//   review): the band scan records one provenance per object it roots, and
//   the line prints it beside whichever entry names that object -- so a
//   `cat=11: Root snapshot .../jit-shadow-stack` holder carrying a band
//   `prov=` means the same object was ALSO a band word of that frame (the
//   `aliases` list shows the section-14 entry), not that the shadow-stack
//   entry itself sits at that offset. On `d8a690353` both were the same
//   frame's dead reference home: the IR frame block publishes every home once
//   per activation and the band scan reads every word below the watermark.
// * gen r5w6/oomjit10: a register-image word's `prov=` also names the
//   register and what kept it: ` reg=rax mask=0x..` (or `mask=none`) in
//   `safepoint-gpr-spill-image` -- the frame's own register oop mask at its
//   active safepoint -- and ` reg=rbx caller=<native|jit:method> claim=Keep`
//   in `callee-saved-gpr-image` -- whose value it is, and the verdict of
//   `conservative_roots::caller_register_claim` (`caller=native` is the VM's
//   Rust code at the interpreter->JIT boundary, which no claim can speak for).
// * The provenance map is reset only at the start of a collection's own root
//   scan (`gc_quiescence::clear_pinned_jit_roots`), and while this census is
//   on EVERY band scan records into it (`record_provenance_for`), a snapshot
//   deposit's included; a `prov=` naming a frame that cannot be live at this
//   collection therefore came from some other scan since that reset.

/// At most this many markers are explained per major (largest closures first).
const HOLDER_MARKERS_MAX: usize = 4;
/// At most this many holders are named per marker.
const HOLDER_ROUNDS_MAX: usize = 6;
/// Path elements printed per holder; a longer path keeps both ends.
const HOLDER_PATH_MAX: usize = 10;
/// Other root entries naming a holder's object, printed per holder.
const HOLDER_ALIASES_MAX: usize = 6;
/// Objects walked for a marker's `reach_bytes` (its own closure).
const HOLDER_REACH_WALK_MAX: usize = 1 << 20;

/// One object the VM asks the census to explain.
#[derive(Clone, Debug)]
pub struct HolderMarker {
    /// The object's address (resolved to its containing object).
    pub addr: usize,
    /// Why the VM watches it, printed verbatim.
    pub why: String,
}

thread_local! {
    /// The markers the next censused major on this thread explains. See
    /// [`stage_holder_markers`].
    static MARKERS: RefCell<Vec<HolderMarker>> = const { RefCell::new(Vec::new()) };
}

/// RAII scope for [`stage_holder_markers`]: restores the previous staging on
/// drop, so a nested collection cannot inherit the outer door's markers.
pub struct StagedHolderMarkers {
    prev: Vec<HolderMarker>,
}

impl Drop for StagedHolderMarkers {
    fn drop(&mut self) {
        let prev = std::mem::take(&mut self.prev);
        let _ = MARKERS.try_with(|m| *m.borrow_mut() = prev);
    }
}

/// Stage the markers the next collection on this thread explains (the VM's
/// collection door, under the census token). Hold the guard across the
/// collection.
pub fn stage_holder_markers(markers: Vec<HolderMarker>) -> StagedHolderMarkers {
    let prev = MARKERS
        .try_with(|m| std::mem::replace(&mut *m.borrow_mut(), markers))
        .unwrap_or_default();
    StagedHolderMarkers { prev }
}

/// How a holder search first reached an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    /// Seeded from root entry `index`.
    Root(usize),
    /// Seeded as a young from-space object.
    YoungSeed,
    /// Reached through a reference slot of this object base.
    Parent(usize),
}

/// The seed a search reached `target` from, and the object chain from that
/// seed's object to `target` (both ends included). `(None, [target])` when
/// `target` was not reached.
fn holder_path(via: &FxHashMap<usize, Via>, target: usize) -> (Option<Via>, Vec<usize>) {
    let mut chain = vec![target];
    let mut cur = target;
    let mut seed = None;
    // Every step follows a `Parent` edge the search recorded, which always
    // points at an object recorded EARLIER, so the walk ends; the bound is a
    // backstop against a malformed map.
    for _ in 0..=via.len() {
        match via.get(&cur) {
            Some(&Via::Parent(p)) => {
                chain.push(p);
                cur = p;
            }
            Some(&other) => {
                seed = Some(other);
                break;
            }
            None => break,
        }
    }
    chain.reverse();
    (seed, chain)
}

/// `name(b)` for each object of `chain`, joined by ` -> `, eliding the middle
/// of a chain longer than [`HOLDER_PATH_MAX`].
fn render_chain(chain: &[usize], name: impl Fn(usize) -> String) -> String {
    let piece = |b: usize| format!("{}@0x{b:x}", name(b));
    if chain.len() <= HOLDER_PATH_MAX {
        return chain.iter().map(|&b| piece(b)).collect::<Vec<_>>().join(" -> ");
    }
    let head = HOLDER_PATH_MAX / 2;
    let tail = HOLDER_PATH_MAX - head;
    let mut parts: Vec<String> = chain[..head].iter().map(|&b| piece(b)).collect();
    parts.push(format!("... {} more ...", chain.len() - head - tail));
    parts.extend(chain[chain.len() - tail..].iter().map(|&b| piece(b)));
    parts.join(" -> ")
}

/// The class name of the object at grid base `base`.
fn base_class_name(base: usize) -> String {
    // SAFETY: every caller passes an object base from one of the census grids
    // (via `Walk::resolve`), whose first header word is in bounds.
    let class_id = unsafe { std::ptr::read(base as *const u32) };
    object_name(base, class_id)
}

/// The JVM descriptor of a primitive array (`[C`, `[B`, ...), from its
/// element tag; `None` for a reference array, an instance or a bad tag.
/// Pure over the two tag bytes, for the tests.
fn primitive_array_descriptor(kind_tag: u8, elem_tag: u8) -> Option<&'static str> {
    use cratonvm_types::ArrayElementType as E;
    if kind_tag != cratonvm_types::ObjectKind::Array as u8 {
        return None;
    }
    Some(match array_element_type_from_tag(elem_tag)? {
        E::Boolean => "[Z",
        E::Char => "[C",
        E::Float => "[F",
        E::Double => "[D",
        E::Byte => "[B",
        E::Short => "[S",
        E::Int => "[I",
        E::Long => "[J",
        E::Reference => return None,
    })
}

/// gce e2/f: the name of the object at `base` whose header class id is
/// `class_id`. A primitive array is named by its element tag: its header
/// class id need not name an array class (a 30 MiB `char[]` from
/// `String.toCharArray` printed as `java/lang/Object`,
/// `docs/known-issues/gc/gce-e1f-oldmark-census-names-a-primitive-array-java-lang-object-20260929.md`).
/// Everything else keeps the class-id name.
fn object_name(base: usize, class_id: u32) -> String {
    // SAFETY: every caller passes an object base from a census grid or a
    // resolved root base, whose header word is in bounds (the grid walk or
    // `Walk::resolve` validated it).
    let (kt, et) = unsafe {
        (
            cratonvm_types::kind_tag_at(base as *const u8),
            cratonvm_types::element_type_tag_at(base as *const u8),
        )
    };
    match primitive_array_descriptor(kt, et) {
        Some(d) => d.to_string(),
        None => crate::collector::class_name_for_diagnostics(class_id),
    }
}

/// Breadth-first search from the whole root slice (and, with `young_seeds`,
/// from every young from-space object after it) that never enters a
/// `blocked` object. With a `target` it stops as soon as that is reached.
/// Returns how each reached object was first reached.
fn holder_bfs(
    walk: &Walk<'_>,
    roots: &[ObjectRef],
    blocked: &FxHashSet<usize>,
    young_seeds: bool,
    target: Option<usize>,
) -> FxHashMap<usize, Via> {
    let mut via: FxHashMap<usize, Via> = FxHashMap::default();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for (index, root) in roots.iter().enumerate() {
        if let Some((base, _, _)) = walk.resolve(root.as_ptr() as usize) {
            if !blocked.contains(&base) && !via.contains_key(&base) {
                via.insert(base, Via::Root(index));
                queue.push_back(base);
            }
        }
    }
    if young_seeds {
        for &(base, _) in walk.young {
            if !blocked.contains(&base) && !via.contains_key(&base) {
                via.insert(base, Via::YoungSeed);
                queue.push_back(base);
            }
        }
    }
    if target.is_some_and(|t| via.contains_key(&t)) {
        return via;
    }
    let mut found: Vec<usize> = Vec::new();
    while let Some(base) = queue.pop_front() {
        found.clear();
        // SAFETY: `base` came from `resolve` or the young grid, i.e. it is a
        // grid object base inside the stopped heap.
        unsafe { visit_refs(base, &mut |r| found.push(r)) };
        for &r in &found {
            if let Some((b, _, _)) = walk.resolve(r) {
                if !blocked.contains(&b) && !via.contains_key(&b) {
                    via.insert(b, Via::Parent(base));
                    if target == Some(b) {
                        return via;
                    }
                    queue.push_back(b);
                }
            }
        }
    }
    via
}

/// Bytes of the objects reachable from `base` (itself included), walking at
/// most [`HOLDER_REACH_WALK_MAX`] objects.
fn closure_bytes(walk: &Walk<'_>, base: usize) -> u64 {
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    let mut stack = vec![base];
    seen.insert(base);
    let mut bytes = 0u64;
    let mut found: Vec<usize> = Vec::new();
    while let Some(b) = stack.pop() {
        if let Some((_, size, _)) = walk.resolve(b) {
            bytes += size as u64;
        }
        if seen.len() >= HOLDER_REACH_WALK_MAX {
            break;
        }
        found.clear();
        // SAFETY: `b` is `base` (a resolved grid base) or came from `resolve`.
        unsafe { visit_refs(b, &mut |r| found.push(r)) };
        for &r in &found {
            if let Some((nb, _, _)) = walk.resolve(r) {
                if seen.insert(nb) {
                    stack.push(nb);
                }
            }
        }
    }
    bytes
}

/// Smallest OLD object [`auto_holder_markers`] explains.
const AUTO_MARKER_MIN_BYTES: usize = 1 << 20;

/// gen r5w6/oomjit10: the markers a major explains when the VM staged none.
///
/// The VM stages the referents of the active `WeakReference`s, so a probe
/// that watches nothing that way -- `NativeGrowthReclaimProbe`, whose rounds
/// drop plain arrays and then fail an allocation -- printed no holder block
/// at all, and its retention could only be read off the first-reach category
/// table, which names ONE root per object. This picks the
/// [`HOLDER_MARKERS_MAX`] largest old objects of at least
/// [`AUTO_MARKER_MIN_BYTES`] that the root slice reaches (`walk.visited` as
/// the roots loop left it), largest first, so every root keeping a big dead
/// array is named with its path. Diagnostics only: the census token gates it,
/// like the rest of this module.
fn auto_holder_markers(walk: &Walk<'_>) -> Vec<HolderMarker> {
    let mut big: Vec<(usize, usize)> = walk
        .old
        .iter()
        .map(|&(p, s)| (p as usize, s))
        .filter(|&(base, size)| size >= AUTO_MARKER_MIN_BYTES && walk.visited.contains(&base))
        .collect();
    big.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    big.truncate(HOLDER_MARKERS_MAX);
    big.into_iter()
        .map(|(addr, size)| HolderMarker {
            addr,
            why: format!("auto: a root-reached old object of {size} bytes; no marker was staged"),
        })
        .collect()
}

/// The holder census's report lines for one major (see the section comment).
#[allow(clippy::too_many_arguments)]
fn holder_census(
    seq: u64,
    roots: &[ObjectRef],
    walk: &Walk<'_>,
    tail: Option<(usize, &'static str)>,
    sections: &[(usize, &'static str)],
    vm_labels: &FxHashMap<usize, &'static str>,
    jit_scan: &FxHashSet<usize>,
    markers: &[HolderMarker],
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut resolved: Vec<(usize, bool, &HolderMarker)> = Vec::new();
    for m in markers {
        if let Some((base, _, is_old)) = walk.resolve(m.addr) {
            if !resolved.iter().any(|&(b, _, _)| b == base) {
                resolved.push((base, is_old, m));
            }
        }
    }
    let unblocked: FxHashSet<usize> = FxHashSet::default();
    let full = holder_bfs(walk, roots, &unblocked, false, None);
    let mut reached: Vec<(usize, bool, &HolderMarker, u64)> = resolved
        .iter()
        .filter(|&&(b, _, _)| full.contains_key(&b))
        .map(|&(b, is_old, m)| (b, is_old, m, closure_bytes(walk, b)))
        .collect();
    reached.sort_by(|a, b| b.3.cmp(&a.3));
    lines.push(format!(
        "[holder-census] major #{seq} markers staged={} resolved={} reached_from_roots={} \
         explained={}",
        markers.len(),
        resolved.len(),
        reached.len(),
        reached.len().min(HOLDER_MARKERS_MAX),
    ));
    let root_label = |index: usize| -> String {
        let addr = roots[index].as_ptr() as usize;
        let (kind, detail) = label_for(index, addr, tail, sections, vm_labels, jit_scan);
        let prov = crate::gc_quiescence::lookup_jit_root_provenance(addr)
            .map(|p| format!(" prov=\"{p}\""))
            .unwrap_or_default();
        format!("root[{index}]=0x{addr:x} cat={kind}/{detail}{prov}")
    };
    for (k, &(base, is_old, marker, bytes)) in reached.iter().take(HOLDER_MARKERS_MAX).enumerate() {
        lines.push(format!(
            "[holder-census]   marker#{} 0x{base:x} {} {} reach_bytes={bytes} ({})",
            k + 1,
            base_class_name(base),
            if is_old { "old" } else { "young" },
            marker.why,
        ));
        let mut blocked: FxHashSet<usize> = FxHashSet::default();
        let mut named = 0usize;
        let mut exhausted = false;
        for round in 0..HOLDER_ROUNDS_MAX {
            let searched;
            let via: &FxHashMap<usize, Via> = if round == 0 {
                &full
            } else {
                searched = holder_bfs(walk, roots, &blocked, false, Some(base));
                &searched
            };
            let (seed, chain) = holder_path(via, base);
            let Some(Via::Root(index)) = seed else {
                exhausted = true;
                break;
            };
            named += 1;
            let holder = chain.first().copied().unwrap_or(base);
            lines.push(format!(
                "[holder-census]     holder#{named} {} path: {}",
                root_label(index),
                render_chain(&chain, base_class_name),
            ));
            let mut aliases: Vec<String> = Vec::new();
            let mut n_alias = 0usize;
            for (j, r) in roots.iter().enumerate() {
                if j != index
                    && walk
                        .resolve(r.as_ptr() as usize)
                        .is_some_and(|(b, _, _)| b == holder)
                {
                    n_alias += 1;
                    if aliases.len() < HOLDER_ALIASES_MAX {
                        aliases.push(root_label(j));
                    }
                }
            }
            if n_alias > 0 {
                lines.push(format!(
                    "[holder-census]       holder#{named}'s object is also root entry x{n_alias}: {}",
                    aliases.join("; "),
                ));
            }
            blocked.insert(holder);
        }
        if !exhausted {
            lines.push(format!(
                "[holder-census]     marker#{}: stopped at {HOLDER_ROUNDS_MAX} holders; more may exist",
                k + 1,
            ));
            continue;
        }
        lines.push(format!(
            "[holder-census]     marker#{}: {named} holder(s); with their objects blocked no root reaches it",
            k + 1,
        ));
        if is_old {
            let seeded = holder_bfs(walk, roots, &blocked, true, Some(base));
            if let (Some(Via::YoungSeed), chain) = holder_path(&seeded, base) {
                lines.push(format!(
                    "[holder-census]     marker#{}: ...but young from-space reaches it (the major seeds \
                     from all of it): {}",
                    k + 1,
                    render_chain(&chain, base_class_name),
                ));
            }
        }
    }
    lines
}

/// A VM-side labeller: calls `out(address, label)` for every heap address a
/// VM-owned structure of the COLLECTING thread holds. Runs inside the pause,
/// on the collecting thread, before the mark.
pub type VmRootLabeller = fn(&mut dyn FnMut(usize, &'static str));

/// Diagnostic only: installed once by the VM, read only under the token.
static VM_LABELLER: std::sync::OnceLock<VmRootLabeller> = std::sync::OnceLock::new();

/// Install the VM-side labeller. First call wins, as with the other
/// diagnostic doorways (`gc_quiescence::install_root_source_hook`).
pub fn install_vm_labeller(f: VmRootLabeller) {
    let _ = VM_LABELLER.set(f);
}

/// STW majors this process has censused, for the report's `#n`.
static MAJORS_CENSUSED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// `(first index, label)` of the tail a seeding site appended to the root
    /// slice it hands `old_gen_gc`. See [`TailScope`].
    static TAIL: Cell<Option<(usize, &'static str)>> = const { Cell::new(None) };
    /// The report `old_gen_gc` produced, printed by the end-of-major hook
    /// with the post-sweep occupancy.
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Labels the entries a seeding site APPENDS to the caller's roots before it
/// calls `old_gen_gc` — `roots[start..]` are `label`. Restores the previous
/// value on drop, so a nested collection cannot inherit it.
pub(super) struct TailScope {
    prev: Option<(usize, &'static str)>,
}

impl TailScope {
    pub(super) fn new(start: usize, label: &'static str) -> Self {
        let prev = TAIL.with(|t| t.replace(Some((start, label))));
        Self { prev }
    }
}

impl Drop for TailScope {
    fn drop(&mut self) {
        let prev = self.prev;
        TAIL.with(|t| t.set(prev));
    }
}

/// `(kind, detail)` — printed as `kind/detail`.
type Label = (&'static str, &'static str);

/// Per-category counters.
#[derive(Default, Clone, Copy)]
struct CatStats {
    roots: u64,
    resolved: u64,
    old_objs: u64,
    old_bytes: u64,
    young_bytes: u64,
}

/// One root's own contribution, for the top-roots list.
#[derive(Clone, Copy)]
struct TopRoot {
    index: usize,
    addr: usize,
    label: Label,
    base: usize,
    base_is_old: bool,
    class_id: u32,
    old_bytes: u64,
    young_bytes: u64,
}

/// The object containing `a` in an ascending `(base, size)` grid.
fn containing(grid: &[(usize, usize)], a: usize) -> Option<(usize, usize)> {
    let i = grid.partition_point(|&(b, _)| b <= a);
    if i == 0 {
        return None;
    }
    let (b, s) = grid[i - 1];
    (a < b + s).then_some((b, s))
}

/// Young from-space object grid: `(base, size)` ascending, skipping free
/// blocks and the reserved TLAB tails in `young_skips`, striding GAP-filler
/// sentinels. A stretch it cannot parse is skipped to the next free block and
/// counted (the second value); the census is a diagnostic, so it prefers a
/// short grid to a guessed one.
fn young_grid(young_from: &Arena, young_skips: &[(usize, usize)]) -> (Vec<(usize, usize)>, usize) {
    let mut free_blocks = young_from.free_blocks_sorted();
    if !young_skips.is_empty() {
        free_blocks.extend_from_slice(young_skips);
        free_blocks.sort_by_key(|&(off, _)| off);
    }
    let mut free_iter = free_blocks.iter().peekable();
    let base = young_from.base_ptr() as usize;
    let used = young_from.used();
    let mut out = Vec::new();
    let mut resyncs = 0usize;
    let mut cursor = 0usize;
    while cursor < used {
        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
            continue;
        }
        if cursor >= used || used - cursor < 8 {
            break;
        }
        let p = base + cursor;
        // SAFETY: `cursor + 8 <= used`, so the first header word lies inside
        // from-space's allocated prefix; a plain `u32` read has no validity
        // requirement beyond being in bounds.
        let class_word = unsafe { std::ptr::read(p as *const u32) };
        if class_word == crate::tlab::GAP_FILLER_CLASS_ID.as_u32() {
            // SAFETY: offset 4 lies inside the same 8 in-bounds bytes.
            let gap = unsafe { std::ptr::read((p + 4) as *const u32) } as usize;
            if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                cursor += gap;
                continue;
            }
        }
        let size = if used - cursor >= HEADER_SIZE {
            // SAFETY: `cursor + HEADER_SIZE <= used`: the whole header is in
            // bounds. The two tag bytes are validated BEFORE a `&ObjectHeader`
            // is formed, as `scan_object_for_old_refs` does.
            let tags_ok = unsafe {
                object_kind_from_tag(cratonvm_types::kind_tag_at(p as *const u8)).is_some()
                    && array_element_type_from_tag(cratonvm_types::element_type_tag_at(
                        p as *const u8,
                    ))
                    .is_some()
            };
            if tags_ok {
                // SAFETY: in-bounds header with valid tag bytes (above).
                gen_object_total_size(unsafe { &*(p as *const ObjectHeader) })
            } else {
                0
            }
        } else {
            0
        };
        if size < HEADER_SIZE || size & 7 != 0 || cursor + size > used {
            resyncs += 1;
            if !resync_to_next_free_block(&mut cursor, &mut free_iter) {
                break;
            }
            continue;
        }
        out.push((p, size));
        cursor += size;
    }
    (out, resyncs)
}

/// Visit every heap reference `base` holds: its reference slots and its
/// external-overlay edges (the same two edge families the major's BFS
/// follows for a marked object).
///
/// # Safety
/// `base` must be an object base from one of the census grids, i.e. a header
/// the walk that built the grid validated, inside a stopped heap.
unsafe fn visit_refs(base: usize, f: &mut dyn FnMut(usize)) {
    // SAFETY: the caller's contract: `base` is a grid object base, so its
    // header bytes are in bounds.
    let kt = unsafe { cratonvm_types::kind_tag_at(base as *const u8) };
    // SAFETY: as above.
    let et = unsafe { cratonvm_types::element_type_tag_at(base as *const u8) };
    if object_kind_from_tag(kt).is_none() || array_element_type_from_tag(et).is_none() {
        return;
    }
    // SAFETY: valid, in-bounds header (tags checked above).
    let header = unsafe { &*(base as *const ObjectHeader) };
    // SAFETY: `base` is an object base whose extent the grid walk validated,
    // so the slot ranges the header implies are in bounds.
    unsafe {
        for_each_ref_slot(base as *mut u8, header, |r, _| f(r as usize));
    }
    let class_id = header.class_id.as_u32();
    crate::external_roots::with_external_roots_for_owner(base, Some(class_id), |refs| {
        for r in refs {
            f(r.as_ptr() as usize);
        }
    });
}

/// The label for root entry `index` whose address is `addr`.
///
/// With a staged section (gen r4w6/oomjit6) the label is
/// `(section, refinement)`: the section is WHO PUSHED this entry, which is
/// exact, and the refinement is the most specific registry that names the
/// address ([`address_detail`]), or `-`. Without one it is wave 5's
/// address-only label.
fn label_for(
    index: usize,
    addr: usize,
    tail: Option<(usize, &'static str)>,
    sections: &[(usize, &'static str)],
    vm_labels: &FxHashMap<usize, &'static str>,
    jit_scan: &FxHashSet<usize>,
) -> Label {
    if let Some((start, label)) = tail {
        if index >= start {
            return ("seed", label);
        }
    }
    if let Some(section) = section_of(sections, index) {
        return (section, address_detail(addr, vm_labels, jit_scan).unwrap_or("-"));
    }
    if let Some(&label) = vm_labels.get(&addr) {
        return ("vm", label);
    }
    if let Some(source) = crate::gc_quiescence::root_source_of(addr) {
        return ("native-source", source);
    }
    if crate::gc_quiescence::is_unrewritable_jit_root(addr) {
        return ("jit", "band-unrewritable");
    }
    if crate::gc_quiescence::is_movable_jit_root(addr) {
        return ("jit", "precise-movable");
    }
    if jit_scan.contains(&addr) {
        return ("jit", "frame-scan");
    }
    ("vm", "unattributed")
}

/// The most specific registry naming `addr`, as one word, in [`label_for`]'s
/// order: a VM structure, a named native root source, then the JIT scan's
/// registries. `None` when nothing names it.
fn address_detail(
    addr: usize,
    vm_labels: &FxHashMap<usize, &'static str>,
    jit_scan: &FxHashSet<usize>,
) -> Option<&'static str> {
    if let Some(&label) = vm_labels.get(&addr) {
        return Some(label);
    }
    if let Some(source) = crate::gc_quiescence::root_source_of(addr) {
        return Some(source);
    }
    if crate::gc_quiescence::is_unrewritable_jit_root(addr) {
        return Some("jit-band-unrewritable");
    }
    if crate::gc_quiescence::is_movable_jit_root(addr) {
        return Some("jit-precise-movable");
    }
    if jit_scan.contains(&addr) {
        return Some("jit-frame-scan");
    }
    None
}

/// The census traversal state.
struct Walk<'a> {
    old: &'a [(*mut u8, usize)],
    young: &'a [(usize, usize)],
    visited: FxHashSet<usize>,
    stack: Vec<(usize, usize, bool)>,
    /// Reused per visited object instead of a fresh `Vec` each.
    scratch: Vec<usize>,
}

impl Walk<'_> {
    /// `a`'s containing object as `(base, size, is_old)`.
    fn resolve(&self, a: usize) -> Option<(usize, usize, bool)> {
        if a == 0 {
            return None;
        }
        let i = self.old.partition_point(|&(p, _)| (p as usize) <= a);
        if i > 0 {
            let (p, s) = self.old[i - 1];
            let b = p as usize;
            if a < b + s {
                return Some((b, s, true));
            }
        }
        containing(self.young, a).map(|(b, s)| (b, s, false))
    }

    /// Traverse from `a`; returns the `(old objects, old bytes, young bytes)`
    /// newly reached.
    fn from(&mut self, a: usize) -> (u64, u64, u64) {
        let (mut old_objs, mut old_bytes, mut young_bytes) = (0u64, 0u64, 0u64);
        if let Some(obj) = self.resolve(a) {
            if self.visited.insert(obj.0) {
                self.stack.push(obj);
            }
        }
        while let Some((base, size, is_old)) = self.stack.pop() {
            if is_old {
                old_objs += 1;
                old_bytes += size as u64;
            } else {
                young_bytes += size as u64;
            }
            let mut found = std::mem::take(&mut self.scratch);
            found.clear();
            // SAFETY: `base` came from `resolve`, i.e. from one of the grids.
            unsafe { visit_refs(base, &mut |r| found.push(r)) };
            for &r in &found {
                if let Some(obj) = self.resolve(r) {
                    if self.visited.insert(obj.0) {
                        self.stack.push(obj);
                    }
                }
            }
            self.scratch = found;
        }
        (old_objs, old_bytes, young_bytes)
    }
}

/// Run the census over the root slice `old_gen_gc` is about to seed and park
/// the report for [`flush`]. Read-only.
///
/// gcd d9/a: `true_roots` says the major seeds young from the TRUE roots
/// (`gen_heap::TrueRootYoung`), not from all of young from-space. The census
/// still attributes what the LEGACY seed would keep (`young/from-space-unrooted`,
/// "young from-space reaches it"), and says so on one line, so a reader does
/// not take those rows for this major's holders. `roots` then excludes the
/// promotion destinations the sweep appended: the seed resolves them.
pub(super) fn run(
    roots: &[ObjectRef],
    young_from: &Arena,
    old_gen: &OldGen,
    walked_objects: &[(*mut u8, usize)],
    young_skips: &[(usize, usize)],
    compact: bool,
    true_roots: bool,
) {
    let seq = MAJORS_CENSUSED.fetch_add(1, Ordering::Relaxed) + 1;
    let tail = TAIL.with(Cell::get);
    // A copy, not a borrow: the VM labeller below runs VM code, which must be
    // free to stage (and restore) sections of its own without a `RefCell`
    // conflict with this read.
    let sections: Vec<(usize, &'static str)> = SECTIONS
        .try_with(|s| s.borrow().clone())
        .unwrap_or_default();
    let mut vm_labels: FxHashMap<usize, &'static str> = FxHashMap::default();
    let labeller_installed = match VM_LABELLER.get() {
        Some(f) => {
            f(&mut |addr: usize, label: &'static str| {
                vm_labels.entry(addr).or_insert(label);
            });
            true
        }
        None => false,
    };
    let jit_scan: FxHashSet<usize> = crate::gc_quiescence::pinned_jit_roots_snapshot()
        .into_iter()
        .collect();
    let (young, resyncs) = young_grid(young_from, young_skips);
    let mut walk = Walk {
        old: walked_objects,
        young: &young,
        visited: FxHashSet::default(),
        stack: Vec::new(),
        scratch: Vec::new(),
    };

    let mut cats: FxHashMap<Label, CatStats> = FxHashMap::default();
    let mut top: Vec<TopRoot> = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let addr = root.as_ptr() as usize;
        let label = label_for(index, addr, tail, &sections, &vm_labels, &jit_scan);
        let stats = cats.entry(label).or_default();
        stats.roots += 1;
        let Some((base, _, base_is_old)) = walk.resolve(addr) else {
            continue;
        };
        stats.resolved += 1;
        let (o, ob, yb) = walk.from(addr);
        stats.old_objs += o;
        stats.old_bytes += ob;
        stats.young_bytes += yb;
        if ob > 0 {
            // SAFETY: `base` is a grid object base (from `resolve`); its
            // first header word is in bounds.
            let class_id = unsafe { std::ptr::read(base as *const u32) };
            top.push(TopRoot {
                index,
                addr,
                label,
                base,
                base_is_old,
                class_id,
                old_bytes: ob,
                young_bytes: yb,
            });
            if top.len() > 64 {
                top.sort_by(|a, b| b.old_bytes.cmp(&a.old_bytes));
                top.truncate(8);
            }
        }
    }
    // gen r5w6/oomjit10: taken HERE, while `walk.visited` is exactly what the
    // root slice reaches (the young walk below adds the unrooted young).
    let auto_markers = auto_holder_markers(&walk);
    // Everything young the roots did not reach: the major seeds old gen from
    // the whole of young from-space regardless.
    let mut unrooted_entries: Vec<(usize, u64, u64)> = Vec::new();
    {
        let label: Label = ("young", "from-space-unrooted");
        let mut agg = CatStats::default();
        for &(base, _) in &young {
            if walk.visited.contains(&base) {
                continue;
            }
            agg.roots += 1;
            let (o, ob, yb) = walk.from(base);
            agg.old_objs += o;
            agg.old_bytes += ob;
            agg.young_bytes += yb;
            if ob > 0 {
                unrooted_entries.push((base, ob, yb));
            }
        }
        agg.resolved = agg.roots;
        cats.insert(label, agg);
    }
    // gen r4w6 (orchestrator): WHO holds the largest unrooted young entry
    // points. Each is a young object no root reaches that the major still
    // seeds from; its referrers (old or young, by class) say which edge the
    // young collection kept it through. One linear pass over both grids per
    // entry, diagnostics only.
    unrooted_entries.sort_by(|a, b| b.1.cmp(&a.1));
    unrooted_entries.truncate(3);
    let mut unrooted_lines: Vec<String> = Vec::new();
    for &(entry, ob, yb) in &unrooted_entries {
        // SAFETY: `entry` is a young grid base; its first header word is in
        // bounds.
        let class_id = unsafe { std::ptr::read(entry as *const u32) };
        let end = containing(&young, entry).map_or(entry + 8, |(b, s)| b + s);
        let mut referrers: Vec<String> = Vec::new();
        let mut n_ref = 0usize;
        let grids = walked_objects
            .iter()
            .map(|&(p, s)| (p as usize, s, true))
            .chain(young.iter().map(|&(b, s)| (b, s, false)));
        for (b, _s, is_old) in grids {
            let mut hit = false;
            // SAFETY: `b` is a grid base of one of the two walks.
            unsafe {
                visit_refs(b, &mut |r| {
                    if r >= entry && r < end {
                        hit = true;
                    }
                })
            };
            if hit {
                n_ref += 1;
                if referrers.len() < 4 {
                    // SAFETY: as above.
                    let rc = unsafe { std::ptr::read(b as *const u32) };
                    referrers.push(format!(
                        "{} 0x{b:x} {}",
                        if is_old { "old" } else { "young" },
                        object_name(b, rc)
                    ));
                }
            }
        }
        let in_roots = roots.iter().any(|r| {
            let a = r.as_ptr() as usize;
            a >= entry && a < end
        });
        unrooted_lines.push(format!(
            "[oldmark-root-census]   unrooted young 0x{entry:x} {} old_bytes={ob} young_bytes={yb} \
             referrers={n_ref} in_roots={in_roots} [{}]",
            object_name(entry, class_id),
            referrers.join("; "),
        ));
    }

    // gen r5w4/jit8: the holder census, over the same slice and labels. A
    // copy of the staging, as for `sections`.
    let staged: Vec<HolderMarker> = MARKERS.try_with(|m| m.borrow().clone()).unwrap_or_default();
    // gen r5w6/oomjit10: no staged marker -> explain the largest root-reached
    // old objects instead (`auto_holder_markers`).
    let markers = if staged.is_empty() {
        auto_markers
    } else {
        staged
    };
    let holder_lines = if markers.is_empty() {
        Vec::new()
    } else {
        holder_census(
            seq, roots, &walk, tail, &sections, &vm_labels, &jit_scan, &markers,
        )
    };

    top.sort_by(|a, b| b.old_bytes.cmp(&a.old_bytes));
    top.truncate(8);
    let mut rows: Vec<(Label, CatStats)> = cats.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.old_bytes
            .cmp(&a.1.old_bytes)
            .then(b.1.roots.cmp(&a.1.roots))
    });
    let census_old_bytes: u64 = rows.iter().map(|(_, s)| s.old_bytes).sum();

    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "[oldmark-root-census] major #{seq} arm={} roots={} old_used_before={} old_objects={} \
         census_old_bytes={census_old_bytes} young_objects={} young_walk_resyncs={resyncs} \
         vm_labeller={} vm_labelled_addrs={} jit_scan_addrs={} jit_unrewritable={} \
         jit_movable={} sections={}",
        if compact { "compacting" } else { "in-place" },
        roots.len(),
        old_gen.used(),
        walked_objects.len(),
        young.len(),
        if labeller_installed { "on" } else { "off" },
        vm_labels.len(),
        jit_scan.len(),
        crate::gc_quiescence::unrewritable_jit_root_count(),
        crate::gc_quiescence::movable_jit_root_count(),
        sections.len(),
    );
    if true_roots {
        let _ = writeln!(
            out,
            "[oldmark-root-census]   seed=true-roots: this major seeds young only from what \
             the roots and the marked old objects reach (CRATONVM_GC_FULL_GC_TRUE_ROOTS); the \
             young/from-space-unrooted row and \"young from-space reaches it\" lines below are \
             what the LEGACY seed would keep, not holders of this major",
        );
    }
    // gen r4w6/oomjit6: the section map itself, once per major, so a `cat=`
    // line can be read against the index ranges that produced it.
    for (i, &(start, name)) in sections.iter().enumerate() {
        let end = sections.get(i + 1).map_or(roots.len(), |&(next, _)| next);
        let _ = writeln!(
            out,
            "[oldmark-root-census]   section [{start}..{end}) {name}",
        );
    }
    for ((kind, detail), s) in &rows {
        let _ = writeln!(
            out,
            "[oldmark-root-census]   cat={kind}/{detail} roots={} resolved={} old_objs={} \
             old_bytes={} young_bytes={}",
            s.roots, s.resolved, s.old_objs, s.old_bytes, s.young_bytes,
        );
    }
    for (rank, t) in top.iter().enumerate() {
        let prov = crate::gc_quiescence::lookup_jit_root_provenance(t.addr)
            .map(|p| format!(" prov=\"{p}\""))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "[oldmark-root-census]   top#{} root[{}]=0x{:x} cat={}/{} -> {} 0x{:x} {} \
             old_bytes={} young_bytes={}{prov}",
            rank + 1,
            t.index,
            t.addr,
            t.label.0,
            t.label.1,
            if t.base_is_old { "old" } else { "young" },
            t.base,
            object_name(t.base, t.class_id),
            t.old_bytes,
            t.young_bytes,
        );
    }
    for line in &unrooted_lines {
        let _ = writeln!(out, "{line}");
    }
    for line in &holder_lines {
        let _ = writeln!(out, "{line}");
    }
    PENDING.with(|p| *p.borrow_mut() = Some(out));
}

/// Print the report [`run`] parked, with the occupancy the collection left.
/// Called at the end of every stop-the-world old-gen collection.
pub(super) fn flush(old_used_after: usize) {
    let Some(report) = PENDING.with(|p| p.borrow_mut().take()) else {
        return;
    };
    eprint!("{report}");
    eprintln!("[oldmark-root-census]   end old_live_after={old_used_after}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gce e2/f: a primitive array is named by its element tag; a reference
    /// array, an instance and a bad tag fall back to the class-id name.
    #[test]
    fn gce_e2f_a_primitive_array_is_named_by_its_element_tag() {
        use cratonvm_types::{ArrayElementType as E, ObjectKind as K};
        let arr = K::Array as u8;
        assert_eq!(primitive_array_descriptor(arr, E::Char as u8), Some("[C"));
        assert_eq!(primitive_array_descriptor(arr, E::Byte as u8), Some("[B"));
        assert_eq!(primitive_array_descriptor(arr, E::Long as u8), Some("[J"));
        assert_eq!(primitive_array_descriptor(arr, E::Boolean as u8), Some("[Z"));
        assert_eq!(primitive_array_descriptor(arr, E::Reference as u8), None);
        assert_eq!(primitive_array_descriptor(arr, 0xEE), None, "bad tag");
        assert_eq!(
            primitive_array_descriptor(K::Object as u8, E::Char as u8),
            None,
            "an instance's element bits are hash bits"
        );
    }

    #[test]
    fn containing_resolves_bases_and_interiors_and_rejects_gaps() {
        let grid = vec![(0x1000usize, 0x40usize), (0x2000, 0x10)];
        assert_eq!(containing(&grid, 0x1000), Some((0x1000, 0x40)));
        assert_eq!(containing(&grid, 0x1038), Some((0x1000, 0x40)));
        assert_eq!(containing(&grid, 0x1040), None, "one past the end");
        assert_eq!(containing(&grid, 0x0fff), None, "below the first object");
        assert_eq!(containing(&grid, 0x200f), Some((0x2000, 0x10)));
        assert_eq!(containing(&grid, 0x3000), None);
    }

    /// gen r5w6/oomjit10: with no staged marker the census explains the
    /// largest ROOT-REACHED old objects of at least a mebibyte, largest first.
    /// Nothing is dereferenced, so the grid may name made-up addresses.
    #[test]
    fn auto_markers_are_the_largest_root_reached_old_objects() {
        let at = std::ptr::without_provenance_mut::<u8>;
        let old: Vec<(*mut u8, usize)> = vec![
            (at(0x10_0000), 2 << 20),
            (at(0x40_0000), 8 << 20),
            (at(0x100_0000), 64),
            (at(0x200_0000), 4 << 20),
        ];
        let young: Vec<(usize, usize)> = Vec::new();
        let mut walk = Walk {
            old: &old,
            young: &young,
            visited: FxHashSet::default(),
            stack: Vec::new(),
            scratch: Vec::new(),
        };
        for base in [0x10_0000usize, 0x40_0000, 0x100_0000] {
            walk.visited.insert(base);
        }
        let picked: Vec<usize> = auto_holder_markers(&walk).iter().map(|m| m.addr).collect();
        assert_eq!(
            picked,
            vec![0x40_0000, 0x10_0000],
            "an unreached object and one below the floor are not explained"
        );
    }

    #[test]
    fn the_tail_scope_labels_appended_seeds_and_restores() {
        let empty_vm: FxHashMap<usize, &'static str> = FxHashMap::default();
        let empty_jit: FxHashSet<usize> = FxHashSet::default();
        assert_eq!(TAIL.with(Cell::get), None);
        {
            let _outer = TailScope::new(3, "promotion-dest");
            let tail = TAIL.with(Cell::get);
            assert_eq!(
                label_for(2, 0x5000, tail, &[], &empty_vm, &empty_jit),
                ("vm", "unattributed")
            );
            assert_eq!(
                label_for(3, 0x5000, tail, &[], &empty_vm, &empty_jit),
                ("seed", "promotion-dest")
            );
            // The tail outranks a staged section: the gen heap appended those
            // entries after the door staged its map.
            assert_eq!(
                label_for(3, 0x5000, tail, &[(0, "initiator")], &empty_vm, &empty_jit),
                ("seed", "promotion-dest")
            );
            {
                let _inner = TailScope::new(1, "finalizer-old");
                assert_eq!(TAIL.with(Cell::get), Some((1, "finalizer-old")));
            }
            assert_eq!(TAIL.with(Cell::get), Some((3, "promotion-dest")));
        }
        assert_eq!(TAIL.with(Cell::get), None);
    }

    #[test]
    fn a_vm_label_outranks_the_jit_scan_registry() {
        let mut vm: FxHashMap<usize, &'static str> = FxHashMap::default();
        vm.insert(0x7000, "deopt-stash");
        let mut jit: FxHashSet<usize> = FxHashSet::default();
        jit.insert(0x7000);
        jit.insert(0x8000);
        assert_eq!(label_for(0, 0x7000, None, &[], &vm, &jit), ("vm", "deopt-stash"));
        assert_eq!(label_for(0, 0x8000, None, &[], &vm, &jit), ("jit", "frame-scan"));
    }

    /// gen r4w6/oomjit6: a staged section names every root by the part of
    /// the VM that pushed it, refined by the registry that names its address,
    /// and `-` when none does -- never `vm/unattributed`.
    #[test]
    fn a_staged_section_names_the_root_and_the_address_refines_it() {
        let mut vm: FxHashMap<usize, &'static str> = FxHashMap::default();
        vm.insert(0x7000, "interp-local-dead-by-liveness");
        let mut jit: FxHashSet<usize> = FxHashSet::default();
        jit.insert(0x8000);
        let sections = [(0usize, "initiator"), (10, "registry-snapshots"), (20, "xt-frozen-peer")];
        assert_eq!(
            label_for(3, 0x7000, None, &sections, &vm, &jit),
            ("initiator", "interp-local-dead-by-liveness")
        );
        assert_eq!(
            label_for(10, 0x8000, None, &sections, &vm, &jit),
            ("registry-snapshots", "jit-frame-scan")
        );
        assert_eq!(
            label_for(25, 0x9000, None, &sections, &vm, &jit),
            ("xt-frozen-peer", "-")
        );
        // Below the first staged start: the address-only label.
        assert_eq!(
            label_for(0, 0x9000, None, &[(5, "late")], &vm, &jit),
            ("vm", "unattributed")
        );
    }

    #[test]
    fn section_of_picks_the_last_start_at_or_below_the_index() {
        let s = [(0usize, "a"), (4, "b"), (4, "c"), (9, "d")];
        assert_eq!(section_of(&s, 0), Some("a"));
        assert_eq!(section_of(&s, 3), Some("a"));
        assert_eq!(section_of(&s, 4), Some("c"), "equal starts: the last one wins");
        assert_eq!(section_of(&s, 8), Some("c"));
        assert_eq!(section_of(&s, 1_000), Some("d"));
        assert_eq!(section_of(&[(2, "x")], 1), None);
        assert_eq!(section_of(&[], 0), None);
    }

    #[test]
    fn staged_sections_sort_and_restore_on_drop() {
        let read = || SECTIONS.with(|s| s.borrow().clone());
        assert!(read().is_empty());
        {
            let _outer = stage_root_sections(vec![(7, "b"), (0, "a")]);
            assert_eq!(read(), vec![(0, "a"), (7, "b")], "sorted by start");
            {
                let _inner = stage_root_sections(vec![(0, "nested")]);
                assert_eq!(read(), vec![(0, "nested")]);
            }
            assert_eq!(read(), vec![(0, "a"), (7, "b")], "the outer staging is back");
        }
        assert!(read().is_empty());
    }

    /// gen r5w4/jit8: a holder path is read back from the search's `Via` map
    /// root-object first, and ends at the target.
    #[test]
    fn holder_path_walks_back_to_the_seed() {
        let mut via: FxHashMap<usize, Via> = FxHashMap::default();
        via.insert(0x10, Via::Root(3));
        via.insert(0x20, Via::Parent(0x10));
        via.insert(0x30, Via::Parent(0x20));
        via.insert(0x40, Via::YoungSeed);
        via.insert(0x50, Via::Parent(0x40));
        assert_eq!(holder_path(&via, 0x30), (Some(Via::Root(3)), vec![0x10, 0x20, 0x30]));
        assert_eq!(holder_path(&via, 0x10), (Some(Via::Root(3)), vec![0x10]));
        assert_eq!(holder_path(&via, 0x50), (Some(Via::YoungSeed), vec![0x40, 0x50]));
        assert_eq!(holder_path(&via, 0x99), (None, vec![0x99]), "not reached");
    }

    #[test]
    fn a_long_holder_path_keeps_both_ends() {
        let name = |b: usize| format!("C{b}");
        assert_eq!(render_chain(&[1, 2], name), "C1@0x1 -> C2@0x2");
        let long: Vec<usize> = (1..=25).collect();
        let s = render_chain(&long, name);
        assert!(s.starts_with("C1@0x1 -> "), "{s}");
        assert!(s.ends_with(" -> C25@0x19"), "{s}");
        assert!(s.contains(&format!("... {} more ...", 25 - HOLDER_PATH_MAX)), "{s}");
    }

    #[test]
    fn staged_holder_markers_restore_on_drop() {
        let read = || MARKERS.with(|m| m.borrow().iter().map(|h| h.addr).collect::<Vec<_>>());
        assert!(read().is_empty());
        {
            let _outer = stage_holder_markers(vec![HolderMarker {
                addr: 0x1000,
                why: "outer".into(),
            }]);
            assert_eq!(read(), vec![0x1000]);
            {
                let _inner = stage_holder_markers(Vec::new());
                assert!(read().is_empty());
            }
            assert_eq!(read(), vec![0x1000], "the outer staging is back");
        }
        assert!(read().is_empty());
    }

    #[test]
    fn flush_without_a_report_prints_nothing_and_takes_it_once() {
        PENDING.with(|p| *p.borrow_mut() = None);
        flush(0);
        PENDING.with(|p| *p.borrow_mut() = Some(String::from("x\n")));
        flush(1);
        assert!(PENDING.with(|p| p.borrow().is_none()));
    }
}
