// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10 wave 8, lane `bybci`: every site that FILES into one of the four
//! by-bci deopt publication maps is guarded by `Compiler::may_file_by_bci`.
//!
//! `docs/internal/fixed-bugs/r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md`
//! found that `deopt_box_ptr_by_bci`, `exc_frame_box_ptr_by_bci`,
//! `osr_exit_box_ptr_by_bci` and `precise_npe_action_by_bci` key on a bare
//! `usize`, so a spliced callee and its enclosing method would share one key
//! space with no owner component. The page proposed widening the key to
//! `(owner, bci)`. That is not what landed, and the reasons are argued in the
//! page's resolution: the maps are declared in `jit/src/x64.rs` and
//! `emit_deopt_stubs`' stub tuples would have to carry the owner too, so no
//! single lane can make the change compile; the owner component the page picks
//! (the splice DEPTH) does not distinguish two splices of one callee at two
//! caller pcs, which is the requirement the page itself states; and a widened key
//! is exercised by nothing, because the population is empty four interlocks over.
//!
//! What landed instead is the invariant that makes the bare key correct, stated
//! and enforced: **the bare-bci key space belongs to the method this compile is
//! compiling, and nothing else may file in it.** A publisher that cannot file
//! falls back to the shared, frame-less exit it used before precise frames
//! existed — so the dangerous direction (SUBSTITUTION: a later publisher at the
//! same key skips its own publication and `emit_deopt_stubs` bakes the splice's
//! pointer as an imm64) becomes the benign one (SUPPRESSION: a lost precise
//! frame).
//!
//! # What this file proves, and what it does not
//!
//! This is a TEXTUAL ratchet over `jit/src/x64/`, and it is textual on purpose
//! rather than for want of trying. The behavioural half exists and is a unit test
//! inside the module that owns the state —
//! `x64::deopt_stubs`'s `a_splice_cannot_file_in_the_bare_bci_key_space`, which
//! seeds `inline_callee_scopes`, runs a real `emit_bounds_check`, and asserts the
//! key space stays clean AND that the guard falls back to the shared AIOOBE pad
//! rather than emitting an unguarded access. That test cannot live here:
//! `x64::deopt_stubs` is a private module (`mod deopt_stubs;` in `x64.rs`) and
//! `Compiler` is not exported, so an integration test cannot reach either.
//!
//! What a unit test cannot do is notice a publisher somebody adds LATER, in a
//! file this lane does not own. That is this file's whole job: it counts the
//! filing sites and the guards, and fails if the two stop matching. It proves
//! text, never behaviour, and a reviewer confirms the guard's effect by reading
//! the unit test above.
//!
//! No test here compiles a Java method or runs the compiler: this lane was not
//! permitted to build, test or probe, and every assertion below is about the
//! crate's own source text.

use std::fs;
use std::path::{Path, PathBuf};

/// The four maps whose key carries no OWNER component, spelled as they appear at
/// a filing site. Three key on a bare `usize`; `deopt_box_ptr_by_bci` keys on
/// `(usize, DeoptReason)` since round 10 wave 8's follow-up — a reason is not an
/// owner, and `the_maps_still_have_no_owner_component` says why that is the only
/// sanctioned widening. `precise_npe_action_by_bci` is in the list for the reason the
/// known-issues page gives: it decides which NPE action a stub takes, so a
/// substituted entry is a wrong exception message at best and a wrong action at
/// worst, and it is filed UNCONDITIONALLY past its neighbour's idempotence test.
const BY_BCI_MAPS: [&str; 4] = [
    "deopt_box_ptr_by_bci",
    "exc_frame_box_ptr_by_bci",
    "osr_exit_box_ptr_by_bci",
    "precise_npe_action_by_bci",
];

/// The guard every filing site must be under. Named, rather than matched by
/// shape, so that renaming it breaks this test instead of silently emptying it.
const GUARD: &str = "may_file_by_bci";

fn jit_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn x64_dir() -> PathBuf {
    jit_src().join("x64")
}

/// Every `.rs` file directly under `jit/src/x64/`, plus `jit/src/x64.rs` itself.
///
/// `x64.rs` is included because that is where the maps are DECLARED, and a
/// filing site added there would be exactly as ambiguous as one in a submodule.
fn x64_sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let root = jit_src().join("x64.rs");
    out.push((
        "x64.rs".to_string(),
        fs::read_to_string(&root).expect("jit/src/x64.rs must be readable"),
    ));
    let dir = x64_dir();
    let mut names: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("jit/src/x64/ must be readable")
        .map(|e| e.expect("a readable dir entry").path())
        .filter(|p| p.extension().map(|e| e == "rs").unwrap_or(false))
        .collect();
    names.sort();
    for p in names {
        let name = p
            .file_name()
            .expect("a named file")
            .to_string_lossy()
            .to_string();
        out.push((
            name,
            fs::read_to_string(&p).expect("an x64 source file must be readable"),
        ));
    }
    out
}

/// A `//`-prefixed line is prose, and the prose in these files quotes these map
/// names constantly — the same filter `scripts/check-orphan-instruments.sh` uses,
/// and for the same reason. A `///` doc line is covered by it too.
fn is_prose(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// One filing site: the file, the 1-based line, and the map.
fn filing_sites() -> Vec<(String, usize, String)> {
    let mut sites = Vec::new();
    for (name, src) in x64_sources() {
        for (i, line) in src.lines().enumerate() {
            if is_prose(line) {
                continue;
            }
            for map in BY_BCI_MAPS {
                if line.contains(&format!("{map}.insert(")) {
                    sites.push((name.clone(), i + 1, map.to_string()));
                }
            }
        }
    }
    sites
}

/// The census, frozen. Ten filing sites in four files — `arrays.rs` (4),
/// `deopt_stubs.rs` (4), `op_control.rs` (1), `osr.rs` (1) — which is the set round 10
/// wave 8 walked and guarded one by one.
///
/// **An eleventh is not necessarily a defect** — it is a site whose author has to
/// answer one question: can this run while `inline_callee_scopes` is non-empty? If
/// it can, it must be under the guard. If it cannot, it must be under the guard
/// anyway, because "cannot" is a property of four interlocks in three other files
/// and the next wave may relax any of them. So the fix in both cases is the same
/// one line, and this test's failure message says so.
#[test]
fn every_by_bci_filing_site_is_under_the_owner_guard() {
    let sites = filing_sites();

    // The whole point of the file. A site is "guarded" when the guard's name
    // occurs on a CODE line in its enclosing function — approximated as anywhere
    // in the 40 lines above it. The widest real gap is 24 lines
    // (`arrays.rs::emit_bounds_check`, whose arming condition sits above the
    // comment block explaining it), so 40 is slack rather than a fit.
    //
    // The heuristic's residual weakness is stated rather than hidden: an
    // UNGUARDED site added within 40 lines AFTER a guarded one borrows its
    // neighbour's guard and passes here. The count ratchet below is the backstop
    // for exactly that — a new site changes the count whether or not the window
    // happens to cover it.
    let sources: Vec<(String, String)> = x64_sources();
    let mut unguarded: Vec<String> = Vec::new();
    for (file, line, map) in &sites {
        let src = &sources
            .iter()
            .find(|(n, _)| n == file)
            .expect("the site's file is in the census")
            .1;
        let lines: Vec<&str> = src.lines().collect();
        let lo = line.saturating_sub(40);
        // CODE lines only. Every one of the ten sites carries a comment block
        // that names the guard and explains it, so counting prose would make this
        // check pass on a site that had the explanation and not the call — which
        // is the failure mode a text scan is most likely to have.
        let guarded = lines[lo..*line]
            .iter()
            .any(|l| !is_prose(l) && l.contains(GUARD));
        if !guarded {
            unguarded.push(format!("{file}:{line} files into {map}"));
        }
    }
    assert!(
        unguarded.is_empty(),
        "these sites file into a by-bci deopt publication map without asking \
         `Compiler::{GUARD}` whether the bare-bci key space still has one owner:\
         \n  {}\n\n\
         The key is a bare `usize` and carries no method. Inside a splice the \
         emitter walks a SECOND method's bytecode on the same maps, and the \
         collision is not symmetric: whichever publisher files FIRST wins, and \
         `emit_deopt_stubs` bakes the surviving pointer as an imm64 into the \
         loser's stub (`resolve_baked_point`). A guard in one method deopting \
         through a frame that describes another one is a silent wrong answer.\n\n\
         Add the guard to the site's ARMING condition, not as a wrapper on the \
         `insert` — see `Compiler::may_file_by_bci` and \
         `docs/internal/fixed-bugs/r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md`.",
        unguarded.join("\n  ")
    );

    // A count ratchet beside the property, because the property above is a
    // heuristic (a 40-line window) and a shrinking census is the way it would
    // quietly stop checking anything. If this number changes, say why in the
    // commit; if it drops to zero, the scan broke rather than the tree improving.
    assert_eq!(
        sites.len(),
        10,
        "the by-bci filing census changed (was 10 sites in 4 files). Sites now:\n  {}",
        sites
            .iter()
            .map(|(f, l, m)| format!("{f}:{l} -> {m}"))
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// The guard must test the SPLICE SCOPE STACK, not something that merely
/// correlates with it.
///
/// This is the assertion that would have caught the shape the known-issues page
/// warns about from the other side: a guard written as
/// `!self.precise_exception_frames` would pass every other test in this file and
/// every behavioural test in the tree, because today the two conditions coincide
/// — and it would silently stop guarding the moment a wave relaxed the
/// precise-frames interlock, which is the exact scenario the guard exists for.
#[test]
fn the_guard_reads_the_splice_scope_stack() {
    let src = fs::read_to_string(x64_dir().join("deopt_stubs.rs"))
        .expect("jit/src/x64/deopt_stubs.rs must be readable");
    let at = src
        .find(&format!("fn {GUARD}(&self"))
        .unwrap_or_else(|| panic!("`fn {GUARD}(&self` must exist in x64/deopt_stubs.rs"));
    // The body, bounded generously: the function is short and its diagnostic arm
    // is the only thing after the early return.
    let body = &src[at..src.len().min(at + 1400)];
    assert!(
        body.contains("inline_callee_scopes.len()"),
        "`{GUARD}` must derive its answer from `inline_callee_scopes`, the splice \
         scope stack — the same field `current_bytecode_owner`, `resume_bci_for` \
         and `build_frame_state_at`'s callee-geometry branch read. Anything that \
         merely correlates with being inside a splice (`precise_exception_frames` \
         is the tempting one, because today it coincides) stops guarding the \
         moment the correlation is relaxed, which is precisely the edit this \
         guard is here to survive."
    );
    assert!(
        body.contains("by_bci_key_space_has_one_owner"),
        "`{GUARD}` must delegate the RULE to the free function, which is what \
         keeps the rule testable without a `Compiler` \
         (`only_depth_zero_owns_the_bare_bci_key_space`)."
    );
}

/// The four maps still key on a bare `usize` — except for the one reason
/// component that was deliberately added, and which is NOT an owner.
///
/// If this fails, somebody widened the keys — which is the known-issues page's
/// own proposal and a legitimate thing to do. It is not a regression; it makes
/// this whole file obsolete, and the person who does it should delete it and say
/// so, rather than find the assertions mysteriously still passing because the
/// guard is now redundant.
///
/// `deopt_box_ptr_by_bci` is the one exception, and it is spelled out rather
/// than waved through. Its key is `(usize, DeoptReason)` since round 10 wave 8's
/// follow-up, because three producers file TWO reasons into it (`BoundsCheck`
/// from the loop-header guard block and from two intrinsic arms,
/// `ReceiverTypeChanged` from sixteen) and `emit_deopt_stubs` reads it from two
/// differently-tagged stubs. That is an orthogonal axis to the one this file
/// guards: the reason says WHICH GUARD at a bci, the owner would say WHOSE bci.
/// The owner component is still absent from all four, `may_file_by_bci` is still
/// what makes that safe, and every assertion above still applies unchanged.
#[test]
fn the_maps_still_have_no_owner_component() {
    let src =
        fs::read_to_string(jit_src().join("x64.rs")).expect("jit/src/x64.rs must be readable");
    let mut bare = 0usize;
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if is_prose(line) {
            continue;
        }
        for map in BY_BCI_MAPS {
            // The declaration, not a use. `name:` alone is not enough: the
            // `Compiler::new` literal spells every one of them as
            // `name: FxHashMap::default(),`, so the discriminator is the TYPE —
            // `FxHashMap<`, which only a declaration has. The window is three
            // lines because the widened declaration wraps its type across them.
            if !line.contains(&format!("{map}:")) {
                continue;
            }
            let decl: String = lines[i..(i + 3).min(lines.len())].concat();
            if !decl.contains("FxHashMap<") {
                continue;
            }
            // Whitespace-free, because rustfmt wraps a declaration whose type no
            // longer fits one line and the join then carries its indentation into
            // the middle of the type. Matching on the squeezed form is what makes
            // this scan indifferent to where rustfmt decides to break.
            let squeezed: String = decl.chars().filter(|c| !c.is_whitespace()).collect();
            let bare_key = squeezed.contains("FxHashMap<usize,");
            // The ONE widened key, and the exact widening. Spelled as a literal
            // so that a THIRD component — an owner, say — fails here as loudly
            // as it would have before.
            let reason_key = map == "deopt_box_ptr_by_bci"
                && squeezed.contains("FxHashMap<(usize,crate::deopt::DeoptReason),");
            assert!(
                bare_key || reason_key,
                "`{map}`'s key is no longer a bare `usize` (nor the one sanctioned \
                 `(usize, DeoptReason)` widening of `deopt_box_ptr_by_bci`). If the \
                 keys now carry an OWNER, this whole test file is obsolete — delete \
                 it and say so in the commit, and retire \
                 `docs/internal/fixed-bugs/r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md` \
                 with it. Declaration read: {decl}"
            );
            bare += 1;
        }
    }
    assert_eq!(
        bare, 4,
        "expected to find all four by-bci map declarations in jit/src/x64.rs; \
         found {bare}. A scan that finds fewer is broken, not looking at a tree \
         without the maps."
    );
}

/// The reason component is REACHED, not merely declared: both filing sites in
/// `deopt_stubs.rs` name a reason, and so do both lookups in `emit_deopt_stubs`.
///
/// Textual, like the rest of this file, and for the same reason — the
/// behavioural half is the unit test
/// `one_bci_carries_a_reason_2_and_a_reason_6_snapshot_independently` inside the
/// private `x64::deopt_stubs` module, which an integration test cannot reach.
/// What a unit test cannot notice is a NEW producer or consumer added later that
/// reverts to keying on the bare bci; the map's type makes that a compile error
/// today, but a `(bci, BoundsCheck)` written where `(bci, reason)` belongs
/// compiles fine and re-opens the exact defect. Hence the pair of counts.
#[test]
fn both_lookups_and_both_filings_name_a_reason() {
    let src = fs::read_to_string(x64_dir().join("deopt_stubs.rs"))
        .expect("jit/src/x64/deopt_stubs.rs must be readable");
    let code: Vec<&str> = src.lines().filter(|l| !is_prose(l)).collect();
    let joined = code.join("\n");

    // Both filings. `emit_deopt_snapshot_at_guard` binds its fixed `BoundsCheck`
    // to a local so the point's stamp and the map key cannot drift apart;
    // `snapshot_pre_intrinsic_call` files under the reason it was HANDED, which
    // is what lets sixteen receiver guards and two bounds checks share the map.
    assert_eq!(
        joined
            .matches("deopt_box_ptr_by_bci.insert((bci, reason), box_ptr)")
            .count(),
        2,
        "both `deopt_box_ptr_by_bci` filing sites must key on `(bci, reason)`. A \
         site that keys on the bci alone no longer compiles, but one that \
         hard-codes a reason its point was NOT stamped with does — and that \
         re-opens the defect from the other end."
    );
    assert!(
        joined.contains("contains_key(&(bci, reason))"),
        "`snapshot_pre_intrinsic_call`'s idempotence test must name the reason. A \
         test on the bci alone is the original defect: a second producer with a \
         DIFFERENT reason at the same pc returns early and its point is never built."
    );

    // The two lookups, each naming the reason its stub tag means.
    assert!(
        joined.contains(".get(&(site_pc, crate::deopt::DeoptReason::BoundsCheck))"),
        "`emit_deopt_stubs`' tag-2 arm must look up `BoundsCheck`"
    );
    assert!(
        joined.contains(".get(&(site_pc, crate::deopt::DeoptReason::ReceiverTypeChanged))"),
        "`emit_deopt_stubs`' tag-6 arm must look up `ReceiverTypeChanged`. Two \
         textually identical lookups is what the defect looked like: whichever \
         producer filed first was baked into BOTH stubs."
    );
}
