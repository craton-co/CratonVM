// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// REVIEW-NOTE (2026-09-16), for the owners of the files this change was not
// permitted to edit. **Every item has landed** (round 12 wave 4 check): item 1
// is the `pub mod profile_store;` line in `jit/src/lib.rs`, item 4 the
// `load_if_configured` / `save_if_configured` calls in `vm/src/vm/vm_init.rs`,
// and item 2 was already landed when this was written. Kept as the record
// `docs/jit/pgo-inlining.md` §6 item 9 cites; the line numbers quoted below
// are those of 2026-09-16.
//
// ---------------------------------------------------------------------------
// 1. `jit/src/lib.rs` — the module declaration. REQUIRED; without it this file
//    is not part of the crate at all.
//
//    The module list at `jit/src/lib.rs:88..162` is alphabetical; its relevant
//    stretch reads:
//
//        pub(crate) mod pgo;      // 154 (module deleted round 12 wave 4)
//        pub mod platform;        // 155
//        pub mod profile;         // 156
//        pub mod range_analysis;  // 157   <-- insert ABOVE this line
//        pub mod regalloc;        // 158
//
//    Insert, as the new line 157, exactly:
//
//        pub mod profile_store;
//
//    `profile_store` sorts after `profile` and before `range_analysis`, so
//    that is the one position that keeps the list sorted.
//
// ---------------------------------------------------------------------------
// 2. The flag surface — ALREADY DONE, verified 2026-09-16, nothing to apply.
//    `flag_groups.rs`'s own "the four files, all of them" doc lists four, and
//    all four already carry these two names:
//
//      * `types/src/flag_groups.rs:1415-1416` —
//        E { group: Group::JIT, token: "profile-save", on_key: Some("CRATONVM_JIT_PROFILE_SAVE"), off_key: None, off_word: None, since: "2026-09-16" },
//        E { group: Group::JIT, token: "profile-load", on_key: Some("CRATONVM_JIT_PROFILE_LOAD"), off_key: None, off_word: None, since: "2026-09-16" },
//      * `types/tests/flag-surface.txt:1004-1005` — `CRATONVM_JIT_PROFILE_LOAD`
//        and `CRATONVM_JIT_PROFILE_SAVE`, in sort order between
//        `CRATONVM_JIT_POST_TLAB_HASH_STAMP` and `CRATONVM_JIT_RANGE_BCE`.
//      * `docs/flag-tokens.md:768-769` and
//        `docs/config/flag-inventory.md:1673-1674` — both GENERATED; regenerate
//        with `tools/flag-census/render-tokens.sh` and
//        `tools/flag-census/render-inventory.py` rather than hand-editing.
//
//    The declarations landed BEFORE this file did, which means
//    `tools/flag-census/check-surface.sh` check 5 ("a row with no read site")
//    was red for those two rows and is green again now that this module reads
//    them. Both are read for their VALUE through `flags::runtime_var` — never
//    presence-parsed, since they take a path — so check 4 (a raw `std::env::var`
//    on a declared name) stays green too. If the rows are ever reverted,
//    `types/tests/flag_declaration_guard.rs` goes red on the two literals in
//    `SAVE_PATH_VAR` / `LOAD_PATH_VAR` below, and until they were declared both
//    names were served by a live `getenv` rather than from the latched
//    `VmFlags` snapshot — so `CRATONVM_JIT=profile-load=...` could not reach
//    them and `flags::with_thread_overrides` could not arrange one in a test.
//
// ---------------------------------------------------------------------------
// 3. Nothing. (This slot held the `flag-surface.txt` edit, which item 2 now
//    reports as already landed. Kept as an empty numbered slot so the item
//    numbers in `docs/jit/pgo-inlining.md` §6 item 9, which cites this note,
//    do not silently shift.)
//
// ---------------------------------------------------------------------------
// 4. `vm/` — the call sites. Read rather than guessed: both functions were
//    opened, and the surrounding code is quoted below so the insertion point is
//    unambiguous rather than described.
//
//    (a) LOAD, at `vm/src/vm/vm_init.rs`, in `impl SharedVm { pub fn new(mut
//        config: VmConfig) -> Self }` (the `fn new` at line 1374). The profile
//        gates are turned on at lines 4536-4542 of that function:
//
//            if crate::runtime::env_cache::tier_pgo()
//                || cratonvm_types::flags::runtime_var_os("CRATONVM_TIER_PGO_ALWAYS").is_some()
//            {
//                crate::jit::profile::enable_profiling(true);
//            }
//            crate::jit::profile::enable_receiver_profiling(
//                crate::runtime::env_cache::tier_pgo_receivers(),
//            );
//
//        Immediately AFTER that `enable_receiver_profiling` call, add:
//
//            crate::jit::profile_store::load_if_configured(&vm.jit.profile_store, &|name| {
//                vm.classes
//                    .class_manager
//                    .read()
//                    .find_unique_class_by_name(name)
//                    .map(|id| id.as_u32())
//            });
//
//        (Since format version 2 both seeding doors take a second resolver,
//        the live method's bytecode length -- see "Stale records" in the
//        module doc; the landed call passes
//        `jit_bridge::live_method_code_len` under a non-blocking read.)
//
//        The resolver is a closure because `cratonvm-jit` has no way to turn a
//        class name into a `ClassId` — it does not depend on the VM crate, and
//        `set_class_namer` (`vm_init.rs:4664`) installs only the *reverse*
//        direction, into `cratonvm-gc`.
//
//        `find_unique_class_by_name` (`classloading/src/class_manager.rs:9398`)
//        and NOT `find_class_by_name` (line 9182), for two reasons that are the
//        same reason. Its own doc says it is the "context-free lookup that
//        succeeds only when one loader has defined the requested name ... safe
//        for diagnostics and legacy metadata that genuinely carry no initiating
//        loader" — and a profile file is exactly that: a class NAME with no
//        loader identity attached, because the producing run's loader ids mean
//        no more across processes than its class ids do. When two loaders have
//        defined the name, it answers `None`, the seed is dropped, and
//        `LoadCensus::methods_unresolved_class` counts it. The alternative,
//        `find_class_by_name`, is `#[deprecated]` as loader-blind and would
//        guess. Guessing here would seed one loader's profile into another
//        loader's class of the same name — a milder version of the exact
//        cross-identity error that storing `class_id` would have made, and one
//        this format exists to refuse.
//
//        Whatever is substituted must not load the class as a side effect: a
//        class the replay names but the run never loads has to resolve to
//        `None` and be dropped, not force a load. Forcing loads from a profile
//        file would let the file change what the program does, which the module
//        doc below forbids in as many words.
//
//        Placement after the gates and before any Java executes is load-bearing:
//        the seeds have to be in the store *before* the first frame runs, which
//        is the whole point of replaying them.
//
//    (b) SAVE, at `vm/src/vm/vm_init.rs`, in `impl Drop for Vm` (line 10570) —
//        the only VM-wide shutdown hook in the tree, and already the home of
//        the AOT training flush, the CDS dump and the missing-native audit
//        dump. Put it beside the AOT flush, BEFORE `run_pending_finalizers()`
//        (finalizers still execute Java and would keep moving the counters
//        under the walk) and before `release_vm_native_state`:
//
//            crate::jit::profile_store::report_replay_outcome(&self.shared.jit.profile_store);
//            crate::jit::profile_store::save_if_configured(
//                &self.shared.jit.profile_store,
//                &|class_id| {
//                    self.shared
//                        .classes
//                        .class_manager
//                        .try_read()
//                        .and_then(|cm| {
//                            cm.get_class(crate::classloading::ClassId::new(class_id))
//                                .map(|c| c.name.to_string())
//                        })
//                },
//            );
//
//        (Since format version 3 the save also takes a live-code resolver,
//        and the landed call asks `self.shared`'s own class manager for both,
//        not `class_name_adapter`, which answers from any live VM.)
//
//        `try_read`, not `read`, for the reason `class_name_adapter`
//        (`vm_init.rs:4940`) gives for the same lookup: this runs on a shutdown
//        path that may already hold VM locks, and a diagnostic that can hang is
//        worse than one that reports fewer methods. A method whose class cannot
//        be named is skipped and counted (`SaveCensus::methods_skipped_unnamed`).
//
//        `report_replay_outcome` is listed separately and deliberately: it is
//        the only reader of the confirm/refute census, and a run with
//        `CRATONVM_JIT_PROFILE_LOAD` set but `..._SAVE` unset — the normal
//        shape of an A/B measurement — would otherwise never print it.
//
//    Neither call site can fail the VM. `load_if_configured` and
//    `save_if_configured` return `bool` and swallow every error into a
//    `tracing::warn!`, because of the rule in the module doc below: a profile
//    is an optimization hint, and a bad profile must never be able to change a
//    program's answer — including by refusing to start.

//! Persistent profiles for the **live** profile store ([`crate::profile`]):
//! write it at shutdown, replay it at startup.
//!
//! # What this is for
//!
//! A JIT review put it this way: *"Replay a previous run's `profile.rs` data at
//! startup to pre-seed tier decisions, MIC/PIC receivers and branch hints —
//! Azul ReadyNow's model. For the Spring Boot / Hibernate / Netty benchmarks
//! this repo tracks, warmup dominates."* Every one of those workloads spends
//! its first seconds re-deriving facts that the previous run already knew:
//! which methods are hot, which branch each `if` takes, which class actually
//! shows up at each `invokeinterface`. This module lets a run start from those
//! answers instead of from nothing.
//!
//! `docs/jit/pgo-inlining.md` §5b is the long form of everything below, and §6
//! items 8-11 are the honest list of what this change did *not* establish —
//! starting with the fact that none of it has been measured.
//!
//! # This is a store for the LIVE profile, and only for it
//!
//! `docs/jit/pgo-inlining.md` §1 tabulates two profile models in this crate.
//! The one this module serialises is [`crate::profile`]: the one the
//! interpreter feeds, the one `jit/src/lib.rs` reads for branch hints, MIC
//! seeds and `classify_receiver_shape`, the one whose receiver table is
//! **uncapped** and whose counters are saturating `u32`. The other one is not
//! wired to anything, disagrees with this one about table capacity and counter
//! width, and its own serialiser is part of what is dead about it. This module
//! shares no code with it; the *shape* of its binary format (magic, version,
//! every count bounded twice, every cursor advance checked) is borrowed, and
//! the hardening below is meant to exceed it, because unlike that one this code
//! **will actually read a file**.
//!
//! # Method identity is `(class_name, method_name, descriptor)` strings
//!
//! This is the single most important correctness property of the format, and it
//! is worth being blunt about why.
//!
//! [`crate::profile::MethodKey`] identifies a method by `(class_id,
//! method_name, descriptor)`, and `class_id` is allocated **per VM and per
//! run**. It is a dense index into this process's class manager, handed out in
//! load order. Two runs of the same program load classes in an order that
//! depends on timing, on the class path, on which lambda proxies got minted
//! first — so last run's class 41 is, with probability approaching one, a
//! different class this run. A format that stored `class_id` would therefore
//! not be stale, which is survivable; it would be *systematically wrong*, which
//! is not. It would seed `java.util.HashMap`'s receiver profile into
//! `org.springframework.SomeBean`, and every consumer downstream would be
//! reading a profile of a method it has never heard of.
//!
//! So the file stores names. Both for the method key and — this is the half
//! that is easy to forget — for every **receiver class** inside a receiver
//! profile, which is `class_id`-keyed in the live store for exactly the same
//! reason and is just as meaningless across processes. [`load_into`] takes a
//! `&dyn Fn(&str) -> Option<u32>` resolver and re-resolves every name against
//! *this* run's class ids. A name that resolves to nothing is dropped, not
//! guessed at.
//!
//! **A name is not a complete identity either, and that is a real limit.** A
//! class is identified in the JVM by `(name, defining loader)`, and this format
//! stores only the name — because a loader id is per-run in exactly the way a
//! class id is, so recording one would be the same mistake one level up. The
//! consequence is that a name two loaders have both defined is *ambiguous*, and
//! the rule for an ambiguous name is to **drop it**, not to pick one. The
//! resolver the VM call site passes has to enforce that; the `REVIEW-NOTE`
//! above names the class-manager lookup that does (`find_unique_class_by_name`,
//! which answers `None` when more than one loader has defined the name) and the
//! deprecated loader-blind one that does not. In a JBoss or Spring Boot
//! deployment with per-module loaders this is not a corner case, and the
//! honest accounting is that replay simply does less there: the ambiguous
//! methods are counted in [`LoadCensus::methods_unresolved_class`] and start
//! cold, exactly as they would have without a file.
//!
//! # A bad profile must never be able to change a program's answer
//!
//! That sentence is the contract, and every line in this file obeys it. It has
//! two halves.
//!
//! **The file is hostile input.** It is a path the operator names, so it is at
//! least as trusted as the command line — but it is also a file that a crashed
//! previous run left half-written, that a shared build cache handed over from a
//! different binary, or that someone pointed at `/dev/urandom` to see what
//! happened. A truncated or corrupt file produces an `Err`, never a panic and
//! never a large allocation, and the VM continues with an empty profile. See
//! "Hostile input" below for which bound refuses what.
//!
//! **A replayed number is a hint, not a fact.** Everything this module seeds is
//! something the compiler already treats as revisable — `docs/jit/pgo-inlining.md`
//! §3's table of "use / class / why it is safe" is unchanged by replay, because
//! replay adds no new consumer and no new speculation. Concretely, for an
//! adversarial or merely stale profile:
//!
//! | it can cause | it cannot cause |
//! |---|---|
//! | a method compiled sooner than it deserved (bounded; see [`ReplaySeedPolicy`]) | a method compiled that the compiler would refuse — `compile_gate` is unchanged |
//! | a monomorphic inline cache seeded with the wrong class | a wrong dispatch: the cache's own `CMP DWORD [recv+0], guard_class_id` re-checks the class id at **every** dispatch, so a wrong seed costs one miss |
//! | a speculative inline guarded on the wrong class | a wrong answer: the guard routes every other receiver to normal dispatch, and a failed guard deopts and re-profiles |
//! | a block laid out on the cold path | a wrong branch: the compare still executes |
//! | an unroll factor chosen for a trip count that no longer happens | a wrong trip count: the loop's own test still runs |
//!
//! The row that matters is the second and third. **A replayed receiver type is
//! re-validated by the same guard the live path emits.** Not a similar guard —
//! the same one, emitted by the same code, because the MIC/PIC seeding path in
//! `jit/src/lib.rs` cannot tell a replayed count from a live one and is not
//! taught to. [`crate::profile::ReplayProvenance`] exists so a *census* can
//! tell them apart; nothing in the compiler reads it, and nothing may start
//! reading it in order to skip a check.
//!
//! The honest summary: a stale profile can cause a wrong **speculation**, which
//! deopts, and must not cause a wrong **answer**. Nothing here is permitted to
//! be the first consumer that would make that false.
//!
//! # The format
//!
//! Little-endian throughout. `[u32 len][utf8 bytes]` for every string.
//!
//! ```text
//! [u32 magic   = 0x43525031]          "CRP1" little-endian
//! [u16 version = 3]
//! [u16 reserved = 0]                  must be zero
//! [u32 method_count]
//! per method:
//!   [u32 record_len]                  bytes of the BODY that follows
//!   body:
//!     [u32 len][class_name]           NOT a ClassId. See above.
//!     [u32 len][method_name]
//!     [u32 len][descriptor]
//!     [u32 code_len]                  the method's bytecode length when saved;
//!                                     0 = the saving run did not know it
//!     [u64 code_hash]                 version 3 only: `bytecode_hash` of the
//!                                     live method when saved; 0 = unknown
//!     [u32 invocation_count]
//!     [u32 n_branches]    per: [u32 pc][u32 taken][u32 not_taken]
//!     [u32 n_call_sites]  per: [u32 pc][u32 count]
//!     [u32 n_loops]       per: [u32 pc][u64 backedge_count][u32 entry_count][u64 total_trips]
//!     [u32 n_recv_sites]  per: [u32 pc][u32 n_types]
//!                           per type: [u32 len][class_name][u32 count]
//! ```
//!
//! Two properties of that layout are load-bearing rather than incidental.
//!
//! **The per-method record is length-delimited.** `record_len` is read and
//! bounds-checked first, and the body is then parsed through a cursor over
//! exactly those bytes. So a corrupt count *inside* a record cannot reach past
//! the end of that record: the damage from one bad byte is one method, and the
//! error message names it. The body cursor must also be exhausted — a record
//! with trailing bytes is an error at every version so far. That is the strict reading;
//! relaxing it is how a *future* version would get skip-forward compatibility
//! (an old reader steps over a record whose new fields it cannot parse), and
//! that relaxation belongs with the version bump that needs it, not before.
//!
//! **Serialisation is deterministic.** Methods are sorted by `(class_name,
//! method_name, descriptor)`, every per-bci array by pc, and every receiver
//! table by descending count with ties broken by ascending class name — the
//! same ranking rule `profile::summarize_receivers` and
//! `crate::classify_receiver_shape` use, for the same reason. Two saves of the
//! same store therefore produce identical bytes, which makes the file diffable
//! and the round-trip test an equality rather than a set comparison.
//!
//! # Hostile input
//!
//! Every count that arrives from the file is checked against **two** bounds
//! before anything is allocated on it, because they do different jobs:
//!
//! * a `MAX_*` **cap** — a policy number, rejecting an absurd count in O(1) at
//!   the four bytes that declare it;
//! * a **bytes-remaining** bound, `remaining / MIN_*_BYTES` — the one that
//!   actually holds, because every element of every array has a known minimum
//!   encoded size, so *n* bytes cannot contain more than *n*/min elements
//!   however generous the cap is.
//!
//! Every cursor advance is `checked_add`, so a length near `u32::MAX` cannot
//! wrap `pos + n` past `data.len()` on a 32-bit target and turn a bounds test
//! into a pass. Every `Vec::with_capacity` is additionally clamped to
//! [`PREALLOC_CLAMP`], so even a count that survives both bounds cannot turn
//! one reservation into a large one. A non-UTF-8 string is an `Err`; so are a
//! bad magic, a non-zero reserved field, an unknown version, and a file
//! truncated at any offset.
//!
//! What is **not** checked is most of the *semantics* of a well-formed file:
//! it may claim a receiver count of `u32::MAX`, or describe a method whose
//! bytecode changed without changing length (version 3 refuses that when the
//! record and the seeding door both know the hash). None of that is memory-unsafe,
//! and none of it can change a program's answer, for the reasons in the table
//! above. It can waste compile time, and [`ReplaySeedPolicy`] is what bounds
//! how much. (A repeated bci or method key IS checked: see [`decode`].)
//!
//! # Stale records (version 2)
//!
//! A replayed record is applied by pc, so a record saved against different
//! bytecode puts its hints at the wrong branches. Version 2 carries each
//! method's `code_len`, and both seeding doors ([`load_into_with_policy`],
//! [`seed_pending_replay_for_class`]) take a second resolver answering the
//! LIVE method's length ([`LiveCode`]). A record is dropped, and counted as
//! stale, when the live class declares no such method with bytecode, when its
//! `code_len` differs from the live one, or when any of its pcs lies past the
//! live code (which also covers a `code_len` of 0). A length is not a hash:
//! an edit that keeps the length passes. It is the cheap check, and it catches
//! every edit that adds or removes an instruction.
//!
//! # Exact records (version 3)
//!
//! Version 3 adds `code_hash`, a [`bytecode_hash`] of the method, and a save
//! takes a third resolver: the same `(class id, name, descriptor) ->
//! LiveCode` question, asked of the SAVING run's live classes
//! ([`save_if_configured`]). A record is stamped from it: a record whose
//! recorders never learned the length (only receiver or call-site evidence)
//! gets the live length, and a record recorded against another length than
//! the live one (an obsolete version won the slot) is written as
//! `CODE_LEN_CONFLICT`. A seeding door whose resolver answers
//! [`LiveCode::Code`] then also drops a record whose non-zero hash differs,
//! which catches an edit that keeps the length. A version 2 file is still
//! read, with every hash unknown, so it keeps the length check.
//!
//! # Flags — both default OFF
//!
//! | variable | effect |
//! |---|---|
//! | `CRATONVM_JIT_PROFILE_SAVE=<path>` | write the store to `<path>` at VM shutdown |
//! | `CRATONVM_JIT_PROFILE_LOAD=<path>` | read `<path>` into the store at VM startup |
//!
//! Both take a **value**, so neither is a boolean flag and neither may be
//! presence-parsed: they are read with `cratonvm_types::flags::runtime_var`,
//! and an unset or empty (or all-whitespace) value means **off**. There is no
//! `=1` spelling and no default path — a feature whose default is "write a file
//! somewhere" is a feature that surprises somebody.
//!
//! They are off because **nothing here has been measured**. No claim is made
//! that this reduces warm-up on any benchmark; no such measurement was taken,
//! and none could be in the session that wrote this. What can be said is what
//! the mechanism is able to do: it puts last run's branch bias, receiver shapes,
//! loop trip counts and a bounded fraction of last run's invocation credit into
//! the store before the first frame executes, which is the evidence the
//! optimizing tier otherwise spends its first seconds re-deriving. What a
//! default flip would need is the same interleaved A/B this repository already
//! runs for the Spring Boot / Hibernate / Netty apps, reporting
//! time-to-steady-state with and without `..._LOAD`, plus the
//! [`ReplayOutcome`] census from those runs showing that the replayed shapes
//! were mostly *confirmed* rather than *refuted* — a run whose receiver seeds
//! are refuted is a run paying deopt for nothing.

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::profile::{MethodKey, ProfileStore, BRANCH_BIAS_MIN_SAMPLES};

// ---------------------------------------------------------------------------
// Format constants
// ---------------------------------------------------------------------------

/// `"CRP1"` little-endian. Distinct from the `PGO_MAGIC` of the deleted
/// `pgo.rs` (round 12 wave 4) on purpose: the two formats described different
/// models (see the module doc), and a reader handed a file in the old format
/// must fail at the magic rather than at the first field whose meaning happens
/// to differ.
const MAGIC: u32 = 0x4352_5031;

/// Current format version. Bump it for **any** change to the layout above,
/// including one that only adds a field: a reader is strict about record
/// exhaustion and would report an appended field as a corrupt record, which is
/// the correct refusal but a confusing message.
///
/// Version 2 added the per-method `code_len`. A version 1 file is refused with
/// a message saying so rather than seeded unchecked: its records cannot be
/// matched against the live bytecode, and the next save writes the current
/// version. Version 3 added the per-method `code_hash`; a version 2 file is
/// read with every hash unknown ([`OLDEST_READABLE_VERSION`]).
const VERSION: u16 = 3;

/// The oldest version [`decode`] reads: version 2, whose records carry a
/// length and no hash.
const OLDEST_READABLE_VERSION: u16 = 2;

/// The 16 bits after the version. Zero, and checked, so it is available to a
/// later version as a flags word without a second version bump.
const RESERVED: u16 = 0;

/// Largest number of methods one file may describe.
///
/// A JDK-class application loads tens of thousands of methods; a million is
/// comfortably above any real profile. The bytes-remaining bound is what
/// actually refuses a hostile count — this is what refuses it in O(1).
const MAX_METHODS: usize = 1 << 20;

/// Largest number of per-bci entries (branches, call sites, loops, receiver
/// sites) one method may declare.
///
/// The JVM spec requires `Code_attribute.code_length` to be below 65 536, so a
/// method has at most 65 536 distinct bcis and therefore at most that many
/// entries in any bci-keyed map. This is that bound, not a guess.
const MAX_PER_BCI_ENTRIES: usize = 1 << 16;

/// Largest number of receiver classes one call site may describe.
///
/// # Why the format caps what the live store does not
///
/// `profile::MethodProfile::record_receiver` inserts **every** distinct class
/// it sees, and `docs/jit/pgo-inlining.md` §1 makes that uncappedness
/// load-bearing: because the live store cannot lose a type,
/// `classify_receiver_shape` is allowed to read a one-type map as
/// `Monomorphic`. A file read cannot inherit that property — an unbounded
/// count out of a file is precisely the allocation amplification this module is
/// defending against — so the format caps, and [`serialize`] drops the excess.
///
/// The cap is chosen so the truncation it introduces **cannot flip a shape
/// verdict towards speculation**. Truncation only ever removes types, so the
/// dangerous direction would be a site reading as *less* polymorphic than it
/// is. `INLINE_MEGAMORPHIC_TYPE_CEILING` is 8 (`jit/src/lib.rs`, tabulated in
/// `docs/feature-designs/profile-guided-inlining.md` §2); 4 096 is 512 times that, so any
/// site this cap truncates still arrives with 4 096 types and still classifies
/// as megamorphic, which refuses. A site with 4 096 or fewer types is not
/// truncated at all, and for those the replayed type count is exact.
///
/// [`serialize`] additionally keeps the *highest-count* classes rather than an
/// arbitrary 4 096, so what truncation discards is the tail.
const MAX_RECEIVER_TYPES_PER_SITE: usize = 1 << 12;

/// Largest byte length any single string field may declare. The JVM caps a
/// `CONSTANT_Utf8` at 65 535 bytes, so this is the class-file format's own
/// ceiling rather than a number invented here.
const MAX_STRING_BYTES: usize = u16::MAX as usize;

/// Largest per-method record body, in bytes.
///
/// Sized from what the caps above permit rather than from a round number:
/// 65 536 branches at 12 bytes, 65 536 call sites at 8, 65 536 loops at 24 and
/// 65 536 receiver sites at 8 is about 3.4 MiB before any receiver *types*, and
/// those carry a class name each. 64 MiB is generous for the worst honest
/// record and still an uninteresting allocation. As everywhere else here, the
/// bytes-remaining bound is the one that holds; this refuses the absurd case
/// without reading further.
const MAX_RECORD_BYTES: usize = 1 << 26;

/// Minimum encoded size of one method, including its `record_len` prefix:
/// four bytes of prefix, three empty strings (4 bytes of length each), the
/// code length, the code hash, the invocation count, and four array counts.
const MIN_METHOD_BYTES: usize = 4 + (3 * 4) + 4 + 8 + 4 + (4 * 4);
/// [`MIN_METHOD_BYTES`] for a version 2 record, which has no code hash.
const MIN_METHOD_BYTES_V2: usize = MIN_METHOD_BYTES - 8;
/// `[u32 pc][u32 taken][u32 not_taken]`.
const MIN_BRANCH_BYTES: usize = 4 + 4 + 4;
/// `[u32 pc][u32 count]`.
const MIN_CALL_SITE_BYTES: usize = 4 + 4;
/// `[u32 pc][u64 backedge_count][u32 entry_count][u64 total_trips]`.
const MIN_LOOP_BYTES: usize = 4 + 8 + 4 + 8;
/// `[u32 pc][u32 n_types]`, before the types.
const MIN_RECEIVER_SITE_BYTES: usize = 4 + 4;
/// `[u32 len][u32 count]`, with an empty class name.
const MIN_RECEIVER_TYPE_BYTES: usize = 4 + 4;

/// Upper bound on any single `Vec::with_capacity` made from a file-supplied
/// count. A surviving count is already bounded by the bytes remaining, so this
/// only matters for a large *legitimate* file, where paying a few reallocations
/// is a better trade than trusting one number.
pub const PREALLOC_CLAMP: usize = 1024;

/// Environment variable naming the file to write at VM shutdown. Takes a path;
/// unset or empty means off.
const SAVE_PATH_VAR: &str = "CRATONVM_JIT_PROFILE_SAVE";

/// Environment variable naming the file to read at VM startup. Takes a path;
/// unset or empty means off.
const LOAD_PATH_VAR: &str = "CRATONVM_JIT_PROFILE_LOAD";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a save or load did not happen.
///
/// Every variant is survivable by construction: the caller's contract is to
/// report it and continue with whatever profile it already had, which for a
/// load is an empty one. See [`load_if_configured`], which is the shape of that
/// contract as code so that a call site cannot get it wrong.
#[derive(Debug)]
pub enum ProfileStoreError {
    /// The file could not be read or written.
    Io(std::io::Error),
    /// The bytes are not a profile this version can read. The string names the
    /// bound that refused them and the offset it refused them at.
    Format(String),
}

impl std::fmt::Display for ProfileStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileStoreError::Io(e) => write!(f, "profile store I/O error: {e}"),
            ProfileStoreError::Format(m) => write!(f, "profile store format error: {m}"),
        }
    }
}

impl std::error::Error for ProfileStoreError {}

impl From<std::io::Error> for ProfileStoreError {
    fn from(e: std::io::Error) -> Self {
        ProfileStoreError::Io(e)
    }
}

fn format_err<T>(message: String) -> Result<T, ProfileStoreError> {
    Err(ProfileStoreError::Format(message))
}

// ---------------------------------------------------------------------------
// Replay seeding policy
// ---------------------------------------------------------------------------

/// How much of a replayed number is allowed into the live store.
///
/// # The rule, and the argument for it
///
/// The four axes this module replays are **not** equally dangerous, so they do
/// not get the same treatment. What separates them is one question: *is there
/// something downstream that re-checks this number against reality?*
///
/// * **Invocation counts — nothing re-checks them.** The invocation counter is
///   what decides whether a method is compiled at all. A method compiled on
///   last run's evidence and never called this run is compile time and code
///   cache spent on nothing, and no later event corrects that decision. This is
///   the axis that gets a divisor **and** a ceiling: the seed is
///   `min(recorded / invocation_divisor, invocation_ceiling)`, and
///   [`Self::conservative`] chooses `invocation_ceiling` to be strictly below
///   the gate the counter feeds. So a replayed count **cannot on its own** make
///   any method eligible for any tier: the method still has to make at least
///   `gate - ceiling` real calls this run. At the default gate of 500 the
///   ceiling is 100, so the seed can shorten the warm-up distance by at most
///   20% — and by proportionally less for a method that was not very hot last
///   run, because the divisor preserves the ordering among them.
///
///   The alternative rule considered and rejected was "seed in full, and flag
///   it `from_replay` so the tiering policy can treat it differently". It is a
///   better rule *if* the policy is taught to read the flag. It is a strictly
///   worse rule until then, because the flag has no reader and the full seed
///   does — and a change that is safe only once somebody else edits a file it
///   does not own is a change that is unsafe. The provenance bit exists anyway
///   ([`crate::profile::ReplayProvenance::seeded_invocations`]) so that policy
///   can be written later and can then *raise* the divisor's effect rather than
///   discover it has no data to work with.
///
/// * **Receiver counts — a guard re-checks them at every dispatch.** A seeded
///   MIC is re-checked by `CMP DWORD [recv+0], guard_class_id`; a speculative
///   inline is guarded and deopts. So the cost of being wrong is a miss or a
///   deopt, not an error, and the ceiling here can be high enough to be *useful*:
///   `classify_receiver_shape` refuses to speculate below
///   `INLINE_MIN_SPECULATION_OBSERVATIONS` (250), so a ceiling under 250 would
///   make receiver replay a no-op and the feature pointless. The ceiling is
///   1 000 — four times that floor, enough that a replayed monomorphic site is
///   immediately usable, and low enough that 1 000 live observations of a
///   different class outvote it. At a site hot enough for any of this to
///   matter, 1 000 dispatches is a blink.
///
/// * **Branch counts — nothing re-checks them, but nothing rests on them.** A
///   wrong layout costs a mis-predicted fall-through, never a wrong answer. The
///   ceiling is five times [`BRANCH_BIAS_MIN_SAMPLES`], the threshold
///   `profile.rs` itself uses before it will call a direction "usual". Five
///   times it means a replayed hint is immediately strong enough to be used and
///   is outvoted by roughly a hundred contrary live observations — which is
///   what "a replayed branch hint is exactly as revisable as a live one" has to
///   mean in numbers rather than in prose.
///
/// * **Loop trips — the loop's own test still runs.** `backedge_count` is a
///   hotness number and gets a ceiling; `entry_count` and `total_trips` encode
///   an *average*, which is scale-free, so they are scaled **together** to
///   preserve it. Scaling one without the other would change the hint rather
///   than weaken it.
///
/// [`Self::identity`] disables all of it and exists for the round-trip test,
/// which has to compare what came out against what went in. It is not a
/// supported configuration: it makes a replayed count indistinguishable in
/// magnitude from an earned one, which is the thing every paragraph above is
/// about.
#[derive(Clone, Copy, Debug)]
pub struct ReplaySeedPolicy {
    /// Divide a recorded invocation count by this before seeding it.
    pub invocation_divisor: u32,
    /// Hard ceiling on a seeded invocation count. [`Self::conservative`] keeps
    /// this strictly below the compile gate.
    pub invocation_ceiling: u32,
    /// Hard ceiling on each direction of a seeded branch count.
    pub branch_ceiling: u32,
    /// Hard ceiling on each seeded receiver-class count.
    pub receiver_ceiling: u32,
    /// Hard ceiling on a seeded call-site execution count.
    pub call_site_ceiling: u32,
    /// Hard ceiling on a seeded loop back-edge count.
    pub backedge_ceiling: u64,
    /// Hard ceiling on a seeded loop entry count; `total_trips` is scaled with
    /// it so the average survives.
    pub loop_entry_ceiling: u32,
}

impl ReplaySeedPolicy {
    /// The policy a production replay uses, derived from `gate` — the smallest
    /// invocation threshold the seeded counter feeds.
    ///
    /// `invocation_ceiling` is `gate / 5`, clamped so it is always at least 1
    /// and always strictly below `gate`. The clamp is what makes the claim
    /// "a replayed count cannot on its own make a method eligible" true for
    /// *every* gate, including a `CRATONVM_JIT_THRESHOLD=4` set by somebody
    /// bisecting a compiler bug, where a fixed constant would have handed every
    /// method in the program to the compiler at startup.
    pub fn conservative(gate: u32) -> Self {
        let gate = gate.max(1);
        let ceiling = (gate / 5).max(1).min(gate.saturating_sub(1));
        Self {
            invocation_divisor: 4,
            invocation_ceiling: ceiling,
            branch_ceiling: BRANCH_BIAS_MIN_SAMPLES.saturating_mul(5),
            receiver_ceiling: 1_000,
            call_site_ceiling: 1_000,
            backedge_ceiling: 10_000,
            loop_entry_ceiling: 1_000,
        }
    }

    /// [`Self::conservative`] against the smallest gate this process actually
    /// applies to the invocation counter.
    ///
    /// Two thresholds read the same counter and either can be lowered on its
    /// own, so the seed has to respect the smaller: `CRATONVM_JIT_THRESHOLD`
    /// (the interpreter's cached-invoke gate, default 500) and
    /// `CRATONVM_TIER_C1_THRESHOLD` (the tiered manager's first admission,
    /// default 500, parsed by [`crate::tiered::CompilationPolicy::from_env`]).
    /// Taking the minimum is the only reading under which the guarantee holds
    /// for a run that lowered one of them.
    ///
    /// Both are read through `cratonvm_types::flags::runtime_var` for their
    /// **value**, not their presence.
    pub fn from_env() -> Self {
        let interpreter_gate = cratonvm_types::flags::runtime_var("CRATONVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(500);
        let tier_gate = crate::tiered::CompilationPolicy::from_env().c1_threshold;
        Self::conservative(interpreter_gate.min(tier_gate))
    }

    /// Seed everything exactly as recorded. **Tests only** — see the struct
    /// doc for why this is not a configuration.
    pub fn identity() -> Self {
        Self {
            invocation_divisor: 1,
            invocation_ceiling: u32::MAX,
            branch_ceiling: u32::MAX,
            receiver_ceiling: u32::MAX,
            call_site_ceiling: u32::MAX,
            backedge_ceiling: u64::MAX,
            loop_entry_ceiling: u32::MAX,
        }
    }

    fn seed_invocations(&self, recorded: u32) -> u32 {
        let divided = recorded / self.invocation_divisor.max(1);
        divided.min(self.invocation_ceiling)
    }
}

// ---------------------------------------------------------------------------
// Censuses
// ---------------------------------------------------------------------------

/// What [`serialize`] wrote, and what it could not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveCensus {
    /// Methods the live store held at the moment of the walk.
    pub methods_in_store: u32,
    /// Methods written to the file.
    pub methods_written: u32,
    /// Methods skipped because their `class_id` could not be turned into a
    /// name. Normal and expected for a class unloaded before shutdown; a large
    /// number means the resolver is failing, not that the program is.
    pub methods_skipped_unnamed: u32,
    /// Methods skipped because they held no evidence worth writing.
    pub methods_skipped_empty: u32,
    /// Receiver `(site, class)` pairs written.
    pub receiver_types_written: u32,
    /// Receiver classes dropped because a site exceeded
    /// [`MAX_RECEIVER_TYPES_PER_SITE`]. The lowest-count classes are the ones
    /// dropped; see that constant for why this cannot flip a shape verdict.
    pub receiver_types_dropped_over_cap: u32,
    /// Receiver classes dropped because their `class_id` could not be named.
    pub receiver_types_unnamed: u32,
    /// Bytes produced.
    pub bytes: usize,
}

/// What [`load_into`] seeded, and what it had to drop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadCensus {
    /// `true` when the configured path does not exist. Not an error — see
    /// [`load_from_path`].
    pub file_missing: bool,
    /// Methods the file described.
    pub methods_in_file: u32,
    /// Methods seeded into the live store.
    pub methods_seeded: u32,
    /// Methods whose class name resolved to no class AT LOAD TIME. Since round
    /// 9 wave 2 these are not dropped: they wait for the class's definition
    /// ([`seed_pending_replay_for_class`]). At load -- `SharedVm::new`, before
    /// any Java runs -- this is nearly every application method in the file.
    /// What is still waiting at exit (never defined, or defined by two
    /// loaders) is [`replay_pending_census`].
    pub methods_unresolved_class: u32,
    /// Methods dropped as stale: the live class declares no such method with
    /// bytecode, its bytecode length differs from the record's `code_len`, its
    /// hash differs from the record's `code_hash` (version 3, both known), or
    /// a recorded pc lies past it. See the module doc's "Stale records".
    pub methods_stale_code: u32,
    /// `(site, class)` receiver pairs seeded.
    pub receiver_types_replayed: u32,
    /// Receiver pairs dropped because the receiver class name resolved to no
    /// class when the OWNING method was seeded.
    pub receiver_types_unresolved: u32,
    /// Branch sites seeded.
    pub branch_sites_seeded: u32,
    /// Sum of the seeded invocation credit, **after** [`ReplaySeedPolicy`] has
    /// divided and capped it. Compare against `methods_seeded × the policy's
    /// ceiling` to see how much of the file's evidence the policy refused.
    pub invocations_seeded: u64,
}

/// How the live run has judged the replayed receiver shapes it was given.
///
/// This is the census that says whether replay is *working*. Seeding is easy;
/// seeding things that turn out to be true is the claim. A run whose
/// `receiver_types_refuted` rivals its `receiver_types_confirmed` is a run
/// paying deopt and re-profiling for hints that were wrong, and that is the
/// evidence a default flip would have to rule out.
///
/// Derived by walking `ProfileStore::snapshot_all()`, so it inherits that
/// method's limits: it is not an atomic image across methods, and a class
/// unloaded before the walk takes its methods' tallies with it. See
/// [`crate::profile::ReplayProvenance`] for why the counters live per method
/// rather than in a pair of process globals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Methods carrying replay provenance.
    pub methods_seeded: u32,
    /// Methods where at least one call site's live receiver contradicted the
    /// replayed shape.
    pub methods_shape_contradicted: u32,
    /// `(site, class)` pairs seeded from the file.
    pub receiver_types_replayed: u32,
    /// Seeded pairs the live run has since observed at the same site.
    pub receiver_types_confirmed: u32,
    /// Seeded pairs discarded because the live run saw a different class at
    /// that site first.
    pub receiver_types_refuted: u32,
    /// Seeded pairs with no verdict yet — the site has not executed this run.
    /// A large number here is not a failure; it is the part of the profile the
    /// run has not reached.
    pub receiver_types_unjudged: u32,
    /// Total invocation credit that came from the replay rather than from this
    /// run's calls.
    pub invocations_seeded: u64,
}

// ---------------------------------------------------------------------------
// The deterministic intermediate form
// ---------------------------------------------------------------------------

/// One method as the file describes it: names, not ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MethodRecord {
    class_name: String,
    method_name: String,
    descriptor: String,
    /// The method's bytecode length when saved; `0` = unknown. See the module
    /// doc's "Stale records".
    code_len: u32,
    /// [`bytecode_hash`] of the live method when saved; `0` = unknown (and
    /// always `0` from a version 2 file). See the module doc's "Exact records".
    code_hash: u64,
    invocations: u32,
    /// `(pc, taken, not_taken)`, pc-ordered.
    branches: Vec<(u32, u32, u32)>,
    /// `(pc, count)`, pc-ordered.
    call_sites: Vec<(u32, u32)>,
    /// `(pc, backedge_count, entry_count, total_trips)`, pc-ordered.
    loops: Vec<(u32, u64, u32, u64)>,
    /// `(pc, [(receiver class name, count)])`, pc-ordered; each table ranked by
    /// descending count with ties broken by ascending class name.
    receivers: Vec<(u32, Vec<(String, u32)>)>,
}

impl MethodRecord {
    /// Whether this record carries any evidence at all. An empty one is not
    /// written: it costs 36 bytes and a resolver call at load and says nothing.
    fn has_evidence(&self) -> bool {
        self.invocations > 0
            || !self.branches.is_empty()
            || !self.call_sites.is_empty()
            || !self.loops.is_empty()
            || !self.receivers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Write `[u32 len][utf8]`, truncating at a UTF-8 boundary if the string is
/// longer than [`MAX_STRING_BYTES`].
///
/// Truncating rather than refusing: the caller is at VM shutdown with a real
/// profile in hand, and a 64 KiB class name is not a thing the JVM can produce
/// (`CONSTANT_Utf8` has the same ceiling), so this arm is unreachable through
/// any honest path. Making it a refusal would mean one impossible name losing
/// the whole file. A truncated name simply fails to resolve at load and is
/// counted there.
fn put_str(buf: &mut Vec<u8>, s: &str) {
    let mut bytes = s.as_bytes();
    if bytes.len() > MAX_STRING_BYTES {
        let mut end = MAX_STRING_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        bytes = &s.as_bytes()[..end];
    }
    put_u32(buf, bytes.len() as u32);
    buf.extend_from_slice(bytes);
}

/// A save-side live-code resolver that knows nothing: every record keeps the
/// length its recorders stamped and an unknown hash.
fn no_live_code(_class_id: u32, _name: &str, _descriptor: &str) -> LiveCode {
    LiveCode::Busy
}

/// Collect the live store into the deterministic intermediate form, with no
/// live-code knowledge (see [`no_live_code`]). The tests' shorthand; a save
/// goes through [`serialize_against_live_code`].
#[cfg(test)]
fn collect(
    store: &ProfileStore,
    class_name: &dyn Fn(u32) -> Option<String>,
    census: &mut SaveCensus,
) -> Vec<MethodRecord> {
    collect_records(store, class_name, &no_live_code, census, false)
}

/// The `(code_len, code_hash)` a record is written with, from the length its
/// recorders stamped (`0` unknown, `CODE_LEN_CONFLICT` two versions) and the
/// saving run's live method. See the module doc's "Exact records".
fn stamp_record_code(stamped: u32, live: LiveCode) -> (u32, u64) {
    let (len, hash) = match live {
        LiveCode::Len(len) => (len, 0),
        LiveCode::Code { len, hash } => (len, hash),
        LiveCode::Absent | LiveCode::Busy => return (stamped, 0),
    };
    let Ok(len) = u32::try_from(len) else {
        return (crate::profile::CODE_LEN_CONFLICT, 0);
    };
    if stamped == 0 || stamped == len {
        (len, hash)
    } else {
        // Recorded against another version than the live one (or already
        // conflicted): never seed it.
        (crate::profile::CODE_LEN_CONFLICT, 0)
    }
}

/// Collect the live store into the deterministic intermediate form, with
/// `live_code` answering the saving run's live method (see
/// [`stamp_record_code`]) and `keep_unjudged` choosing whether replayed seeds
/// are counted: the invocation credit, the branch / call-site / loop seeds and
/// a receiver seed that no live observation has judged yet. A save
/// passes `false` (see `ReplayProvenance::unjudged_seed`); the tests' view of
/// a store's content passes `true`.
fn collect_records(
    store: &ProfileStore,
    class_name: &dyn Fn(u32) -> Option<String>,
    live_code: &dyn Fn(u32, &str, &str) -> LiveCode,
    census: &mut SaveCensus,
    keep_unjudged: bool,
) -> Vec<MethodRecord> {
    // One resolver call per distinct class id, not one per method: the resolver
    // the VM passes takes a class-manager lock, and a Spring Boot store holds
    // tens of thousands of methods across a few thousand classes.
    let mut names: FxHashMap<u32, Option<String>> = FxHashMap::default();
    let mut resolve = |id: u32, names: &mut FxHashMap<u32, Option<String>>| -> Option<String> {
        if let Some(cached) = names.get(&id) {
            return cached.clone();
        }
        let resolved = class_name(id);
        names.insert(id, resolved.clone());
        resolved
    };

    let invocations: FxHashMap<u128, u32> =
        store.snapshot_invocation_counts().into_iter().collect();

    let profiles = store.snapshot_all();
    census.methods_in_store = profiles.len().min(u32::MAX as usize) as u32;

    let mut out: Vec<MethodRecord> = Vec::new();
    for (key, profile) in profiles {
        let Some(class) = resolve(key.class_id, &mut names) else {
            census.methods_skipped_unnamed = census.methods_skipped_unnamed.saturating_add(1);
            continue;
        };

        let packed = cratonvm_jit_api::invoc_key_parts(
            key.class_id,
            key.method_name.as_ref(),
            key.descriptor.as_ref(),
        );

        // A save writes what THIS run earned. The invocation counter also
        // holds the replay's credit (`seed_record` adds it with
        // `add_invocations`, and `note_replayed_invocations` records how
        // much), so a method this run never called was written back with its
        // seed and re-credited on every load, never ageing out
        // (r11-tier-profile-replay-residuals item 3, the invocation half; round
        // 11 wave 18). The tests' view (`keep_unjudged`) keeps the credit.
        let provenance = profile.replay_provenance().filter(|_| !keep_unjudged);
        let earned = invocations
            .get(&packed)
            .copied()
            .unwrap_or(0)
            .saturating_sub(provenance.map_or(0, |p| p.seeded_invocations));

        // `code_len` / `code_hash` are stamped below, once the record is
        // known to be written: the live lookup hashes the method.
        let mut record = MethodRecord {
            class_name: class,
            method_name: key.method_name.to_string(),
            descriptor: key.descriptor.to_string(),
            invocations: earned,
            ..MethodRecord::default()
        };

        // Branch, call-site and loop seeds are subtracted the same way (round
        // 11 wave 19, item 3's last half): they have no live verdict, so a save
        // writes only this run's observations and a replayed hint ages out
        // instead of being written back and re-seeded at every load. `provenance`
        // is `None` for the tests' view, which keeps them.
        for (&pc_key, counts) in profile.branches.iter() {
            let Ok(pc) = u32::try_from(pc_key) else { continue };
            let (seeded_taken, seeded_not_taken) =
                provenance.map_or((0, 0), |p| p.seeded_branch(pc_key));
            let taken = counts.taken.saturating_sub(seeded_taken);
            let not_taken = counts.not_taken.saturating_sub(seeded_not_taken);
            if taken == 0 && not_taken == 0 {
                continue;
            }
            record.branches.push((pc, taken, not_taken));
        }
        record.branches.sort_unstable();

        for (&pc_key, &count) in profile.call_sites.iter() {
            let Ok(pc) = u32::try_from(pc_key) else { continue };
            let count = count.saturating_sub(provenance.map_or(0, |p| p.seeded_call_site(pc_key)));
            if count == 0 {
                continue;
            }
            record.call_sites.push((pc, count));
        }
        record.call_sites.sort_unstable();

        for (&pc_key, lp) in profile.loops.iter() {
            let Ok(pc) = u32::try_from(pc_key) else { continue };
            let (seeded_backedges, seeded_entries, seeded_trips) =
                provenance.map_or((0, 0, 0), |p| p.seeded_loop(pc_key));
            let backedge_count = lp.backedge_count.saturating_sub(seeded_backedges);
            let entry_count = lp.entry_count.saturating_sub(seeded_entries);
            let total_trips = lp.total_trips.saturating_sub(seeded_trips);
            if backedge_count == 0 && entry_count == 0 {
                continue;
            }
            record.loops.push((pc, backedge_count, entry_count, total_trips));
        }
        record.loops.sort_unstable();

        for (&pc_key, counts) in profile.receivers.iter() {
            let Ok(pc) = u32::try_from(pc_key) else {
                continue;
            };
            let mut table: Vec<(String, u32)> = Vec::new();
            for (&receiver_id, &count) in counts.iter() {
                // Only what this run observed or confirmed is saved; a seed
                // still awaiting its live verdict is not evidence yet.
                let count = count
                    .saturating_sub(provenance.map_or(0, |p| p.unjudged_seed(pc_key, receiver_id)));
                if count == 0 {
                    continue;
                }
                match resolve(receiver_id, &mut names) {
                    Some(name) => table.push((name, count)),
                    None => {
                        census.receiver_types_unnamed =
                            census.receiver_types_unnamed.saturating_add(1);
                    }
                }
            }
            // One entry per NAME (round 12 wave 1). Two loaders' classes of one
            // name at one site (a shared library called with two webapps'
            // `Impl`) were two entries, and the replay resolves both names to
            // one class and seeded each, so that class carried twice the
            // per-class ceiling. The file names classes, so the name's total is
            // what it can say.
            table.sort_by(|a, b| a.0.cmp(&b.0));
            table.dedup_by(|later, kept| {
                if later.0 == kept.0 {
                    kept.1 = kept.1.saturating_add(later.1);
                    true
                } else {
                    false
                }
            });
            // Descending count, ties by ascending class name. The same ranking
            // rule `profile::summarize_receivers` and `classify_receiver_shape`
            // use, so the file agrees with the compiler about which class is
            // "top" — and so two saves of one store are byte-identical.
            table.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            if table.len() > MAX_RECEIVER_TYPES_PER_SITE {
                let dropped = table.len() - MAX_RECEIVER_TYPES_PER_SITE;
                census.receiver_types_dropped_over_cap = census
                    .receiver_types_dropped_over_cap
                    .saturating_add(dropped.min(u32::MAX as usize) as u32);
                table.truncate(MAX_RECEIVER_TYPES_PER_SITE);
            }
            if table.is_empty() {
                continue;
            }
            census.receiver_types_written = census
                .receiver_types_written
                .saturating_add(table.len().min(u32::MAX as usize) as u32);
            record.receivers.push((pc, table));
        }
        record.receivers.sort_by(|a, b| a.0.cmp(&b.0));

        if !record.has_evidence() {
            census.methods_skipped_empty = census.methods_skipped_empty.saturating_add(1);
            continue;
        }
        let (code_len, code_hash) = stamp_record_code(
            profile.recorded_code_len(),
            live_code(
                key.class_id,
                record.method_name.as_str(),
                record.descriptor.as_str(),
            ),
        );
        record.code_len = code_len;
        record.code_hash = code_hash;
        out.push(record);
    }

    // Ties (one name defined by several loaders) hottest first, then ONE record
    // per name: replay is by name, so every copy used to seed the same method
    // additively (r11-tier-profile-replay-residuals item 1). The loader copies
    // cannot be told apart in the file anyway.
    out.sort_by(|a, b| record_key_order(a, b).then_with(|| b.invocations.cmp(&a.invocations)));
    out.dedup_by(|later, kept| record_key_order(kept, later) == std::cmp::Ordering::Equal);
    if out.len() > MAX_METHODS {
        out.truncate(MAX_METHODS);
    }
    census.methods_written = out.len().min(u32::MAX as usize) as u32;
    out
}

fn encode(records: &[MethodRecord]) -> Vec<u8> {
    encode_as(records, VERSION)
}

/// [`encode`] at `version`; only a test writes anything but [`VERSION`] (a
/// version 2 file, to prove it is still read). A version below 3 drops the
/// hash.
fn encode_as(records: &[MethodRecord], version: u16) -> Vec<u8> {
    let mut buf = Vec::new();
    put_u32(&mut buf, MAGIC);
    put_u16(&mut buf, version);
    put_u16(&mut buf, RESERVED);
    put_u32(&mut buf, records.len() as u32);

    let mut body = Vec::new();
    for record in records {
        body.clear();
        put_str(&mut body, &record.class_name);
        put_str(&mut body, &record.method_name);
        put_str(&mut body, &record.descriptor);
        put_u32(&mut body, record.code_len);
        if version >= 3 {
            put_u64(&mut body, record.code_hash);
        }
        put_u32(&mut body, record.invocations);

        put_u32(&mut body, record.branches.len() as u32);
        for &(pc, taken, not_taken) in &record.branches {
            put_u32(&mut body, pc);
            put_u32(&mut body, taken);
            put_u32(&mut body, not_taken);
        }

        put_u32(&mut body, record.call_sites.len() as u32);
        for &(pc, count) in &record.call_sites {
            put_u32(&mut body, pc);
            put_u32(&mut body, count);
        }

        put_u32(&mut body, record.loops.len() as u32);
        for &(pc, backedge, entries, trips) in &record.loops {
            put_u32(&mut body, pc);
            put_u64(&mut body, backedge);
            put_u32(&mut body, entries);
            put_u64(&mut body, trips);
        }

        put_u32(&mut body, record.receivers.len() as u32);
        for (pc, table) in &record.receivers {
            put_u32(&mut body, *pc);
            put_u32(&mut body, table.len() as u32);
            for (name, count) in table {
                put_str(&mut body, name);
                put_u32(&mut body, *count);
            }
        }

        put_u32(&mut buf, body.len() as u32);
        buf.extend_from_slice(&body);
    }
    buf
}

/// Serialise the live store, resolving every `class_id` to a name through
/// `class_name`.
///
/// A method whose class cannot be named is skipped and counted; so is a
/// receiver class. See [`SaveCensus`]. This function does no I/O and cannot
/// fail: there is no partial state to leave behind and nothing a caller could
/// usefully do with an error from a pure encode.
pub fn serialize(
    store: &ProfileStore,
    class_name: &dyn Fn(u32) -> Option<String>,
) -> (Vec<u8>, SaveCensus) {
    serialize_against_live_code(store, class_name, &no_live_code)
}

/// [`serialize`], stamping every record's `code_len` / `code_hash` from the
/// saving run's live methods through `live_code` (the module doc's "Exact
/// records"). What [`save_to_path`] writes.
fn serialize_against_live_code(
    store: &ProfileStore,
    class_name: &dyn Fn(u32) -> Option<String>,
    live_code: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> (Vec<u8>, SaveCensus) {
    let mut census = SaveCensus::default();
    let records = collect_records(store, class_name, live_code, &mut census, false);
    let bytes = encode(&records);
    census.bytes = bytes.len();
    (bytes, census)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A bounds-checked cursor over file bytes.
///
/// Every read in the deserialiser goes through this, so the arithmetic has to
/// be right in exactly one place. Two invariants hold for its whole life and
/// the rest of the module relies on both:
///
/// * `self.pos <= self.data.len()`, because the only assignment to `pos` is
///   `pos = end` after `end <= data.len()` has been checked. That is what makes
///   `data.len() - pos` in [`Cursor::count`] a subtraction that cannot
///   underflow.
/// * every advance is `checked_add`, so a length near `u32::MAX` on a 32-bit
///   target cannot wrap the cursor past the end and turn a bounds test into a
///   pass.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    /// Prefixed to every error message, so a failure inside a method record
    /// says which method.
    context: String,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], context: String) -> Self {
        Self {
            data,
            pos: 0,
            context,
        }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn is_exhausted(&self) -> bool {
        self.pos == self.data.len()
    }

    fn fail<T>(&self, message: String) -> Result<T, ProfileStoreError> {
        if self.context.is_empty() {
            format_err(message)
        } else {
            format_err(format!("{}: {message}", self.context))
        }
    }

    fn take(&mut self, want: usize, what: &str) -> Result<&'a [u8], ProfileStoreError> {
        let Some(end) = self.pos.checked_add(want) else {
            return self.fail(format!(
                "declared {what} length {want} overflows the read cursor at offset {}",
                self.pos
            ));
        };
        if end > self.data.len() {
            return self.fail(format!(
                "unexpected end of input reading {what}: wanted {want} byte(s) at offset {}, \
                 {} byte(s) available",
                self.pos,
                self.remaining()
            ));
        }
        // Copy the `&'a [u8]` out of `self` before indexing, so the slice that
        // comes back carries the FILE's lifetime rather than the lifetime of
        // this `&mut self` borrow. `Cursor::sub` depends on that: a
        // length-delimited record's sub-cursor outlives the call that produced
        // it.
        let data: &'a [u8] = self.data;
        let bytes = &data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn u16(&mut self, what: &str) -> Result<u16, ProfileStoreError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32, ProfileStoreError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64, ProfileStoreError> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// An array length, checked against BOTH bounds before the caller may
    /// allocate on it. See the module doc for why one bound is not enough.
    fn count(
        &mut self,
        max: usize,
        min_elem: usize,
        what: &str,
    ) -> Result<usize, ProfileStoreError> {
        let declared = self.u32(what)? as usize;
        if declared > max {
            return self.fail(format!(
                "declared {what} count {declared} exceeds this format's cap of {max}"
            ));
        }
        let remaining = self.remaining();
        let affordable = remaining / min_elem.max(1);
        if declared > affordable {
            return self.fail(format!(
                "declared {what} count {declared} cannot fit in the {remaining} byte(s) \
                 remaining ({min_elem} byte(s) minimum each, so at most {affordable})"
            ));
        }
        Ok(declared)
    }

    fn string(&mut self, what: &str) -> Result<String, ProfileStoreError> {
        let len = self.u32(what)? as usize;
        if len > MAX_STRING_BYTES {
            return self.fail(format!(
                "declared {what} length {len} exceeds the {MAX_STRING_BYTES}-byte cap"
            ));
        }
        let bytes = self.take(len, what)?;
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(s.to_owned()),
            Err(e) => self.fail(format!("{what} is not UTF-8: {e}")),
        }
    }

    /// A sub-cursor over exactly the next `len` bytes, for a length-delimited
    /// record. The parent cursor is advanced past them either way, so a caller
    /// that chooses to skip a malformed record can.
    fn sub(&mut self, len: usize, context: String) -> Result<Cursor<'a>, ProfileStoreError> {
        let bytes = self.take(len, "record body")?;
        Ok(Cursor::new(bytes, context))
    }
}

/// `Vec::with_capacity` clamped to [`PREALLOC_CLAMP`].
fn prealloc<T>(declared: usize) -> Vec<T> {
    Vec::with_capacity(declared.min(PREALLOC_CLAMP))
}

fn decode(data: &[u8]) -> Result<Vec<MethodRecord>, ProfileStoreError> {
    let mut cursor = Cursor::new(data, String::new());
    let magic = cursor.u32("magic")?;
    if magic != MAGIC {
        return cursor.fail(format!(
            "bad magic 0x{magic:08X}, expected 0x{MAGIC:08X} — this is not a CratonVM JIT \
             profile file"
        ));
    }
    let version = cursor.u16("version")?;
    if version == 1 {
        return cursor.fail(format!(
            "profile version 1 carries no per-method code length, so its records cannot be \
             checked against the live bytecode; this build reads version {VERSION}, and the \
             next save writes it"
        ));
    }
    if !(OLDEST_READABLE_VERSION..=VERSION).contains(&version) {
        return cursor.fail(format!(
            "unsupported profile version {version}, this build reads versions \
             {OLDEST_READABLE_VERSION} to {VERSION}"
        ));
    }
    let has_hash = version >= 3;
    let reserved = cursor.u16("reserved")?;
    if reserved != RESERVED {
        return cursor.fail(format!(
            "reserved header field is {reserved}, must be {RESERVED} at version {version}"
        ));
    }

    let min_method_bytes = if has_hash {
        MIN_METHOD_BYTES
    } else {
        MIN_METHOD_BYTES_V2
    };
    let method_count = cursor.count(MAX_METHODS, min_method_bytes, "method")?;
    let mut records: Vec<MethodRecord> = prealloc(method_count);
    for index in 0..method_count {
        let record_len = cursor.count(MAX_RECORD_BYTES, 1, "method record byte")?;
        let mut body = cursor.sub(record_len, format!("method record {index}"))?;
        let record = decode_method(&mut body, has_hash)?;
        if !body.is_exhausted() {
            return body.fail(format!(
                "record declared {record_len} byte(s) but only {} were consumed; at version \
                 {version} a record must be exactly its declared length",
                record_len - body.remaining()
            ));
        }
        // Keys ascend: the writer sorts them. A REPEATED key is dropped, not
        // refused — `collect` wrote one record per loader until round 11 wave 2,
        // and every copy seeded the same method additively, so N loaders
        // defining one class carried N times the invocation credit past the
        // ceilings (r11-tier-profile-replay-residuals item 1). The first copy is
        // the one kept; the writer now puts the hottest first. A key that goes
        // BACKWARDS is no writer's output, so the file is refused.
        if let Some(prev) = records.last() {
            match record_key_order(prev, &record) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => continue,
                std::cmp::Ordering::Greater => {
                    return cursor.fail(format!(
                        "method record {index} ({}.{}{}) is out of order; the writer sorts \
                         records by class, method and descriptor",
                        record.class_name, record.method_name, record.descriptor
                    ));
                }
            }
        }
        records.push(record);
    }
    if !cursor.is_exhausted() {
        return cursor.fail(format!(
            "{} trailing byte(s) after the last of {method_count} method record(s)",
            cursor.remaining()
        ));
    }
    Ok(records)
}

/// The order `collect` writes records in: class, then method, then descriptor.
fn record_key_order(a: &MethodRecord, b: &MethodRecord) -> std::cmp::Ordering {
    a.class_name
        .cmp(&b.class_name)
        .then_with(|| a.method_name.cmp(&b.method_name))
        .then_with(|| a.descriptor.cmp(&b.descriptor))
}

/// Whether `pcs` is strictly ascending. The writer emits every per-bci array
/// sorted by pc from a pc-keyed map, so a repeated pc is no writer's output —
/// and seeding is additive, so a hostile file could otherwise repeat one pc to
/// multiply its credit past the per-entry ceilings.
fn strictly_ascending(pcs: impl Iterator<Item = u32>) -> bool {
    let mut prev: Option<u32> = None;
    for pc in pcs {
        if prev.is_some_and(|p| p >= pc) {
            return false;
        }
        prev = Some(pc);
    }
    true
}

/// One record body; `has_hash` is whether the file's version carries
/// `code_hash` (3 and later). A version 2 record decodes with hash `0`.
fn decode_method(body: &mut Cursor<'_>, has_hash: bool) -> Result<MethodRecord, ProfileStoreError> {
    let class_name = body.string("class name")?;
    let method_name = body.string("method name")?;
    let descriptor = body.string("descriptor")?;
    let code_len = body.u32("code length")?;
    let code_hash = if has_hash { body.u64("code hash")? } else { 0 };
    let invocations = body.u32("invocation count")?;

    let n_branches = body.count(MAX_PER_BCI_ENTRIES, MIN_BRANCH_BYTES, "branch")?;
    let mut branches = prealloc(n_branches);
    for _ in 0..n_branches {
        let pc = body.u32("branch pc")?;
        let taken = body.u32("branch taken count")?;
        let not_taken = body.u32("branch not-taken count")?;
        branches.push((pc, taken, not_taken));
    }

    let n_call_sites = body.count(MAX_PER_BCI_ENTRIES, MIN_CALL_SITE_BYTES, "call site")?;
    let mut call_sites = prealloc(n_call_sites);
    for _ in 0..n_call_sites {
        let pc = body.u32("call site pc")?;
        let count = body.u32("call site count")?;
        call_sites.push((pc, count));
    }

    let n_loops = body.count(MAX_PER_BCI_ENTRIES, MIN_LOOP_BYTES, "loop")?;
    let mut loops = prealloc(n_loops);
    for _ in 0..n_loops {
        let pc = body.u32("loop pc")?;
        let backedge = body.u64("loop back-edge count")?;
        let entries = body.u32("loop entry count")?;
        let trips = body.u64("loop total trips")?;
        loops.push((pc, backedge, entries, trips));
    }

    let n_recv_sites = body.count(
        MAX_PER_BCI_ENTRIES,
        MIN_RECEIVER_SITE_BYTES,
        "receiver site",
    )?;
    let mut receivers = prealloc(n_recv_sites);
    for _ in 0..n_recv_sites {
        let pc = body.u32("receiver site pc")?;
        let n_types = body.count(
            MAX_RECEIVER_TYPES_PER_SITE,
            MIN_RECEIVER_TYPE_BYTES,
            "receiver type",
        )?;
        let mut table = prealloc(n_types);
        for _ in 0..n_types {
            let name = body.string("receiver class name")?;
            let count = body.u32("receiver count")?;
            table.push((name, count));
        }
        receivers.push((pc, table));
    }

    if !strictly_ascending(branches.iter().map(|b: &(u32, u32, u32)| b.0))
        || !strictly_ascending(call_sites.iter().map(|c: &(u32, u32)| c.0))
        || !strictly_ascending(loops.iter().map(|l: &(u32, u64, u32, u64)| l.0))
        || !strictly_ascending(receivers.iter().map(|r: &(u32, Vec<(String, u32)>)| r.0))
    {
        return body.fail(format!(
            "{class_name}.{method_name}{descriptor}: a per-bci table repeats or reorders a \
             pc; the writer emits each strictly ascending"
        ));
    }

    Ok(MethodRecord {
        class_name,
        method_name,
        descriptor,
        code_len,
        code_hash,
        invocations,
        branches,
        call_sites,
        loops,
        receivers,
    })
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// The 64-bit digest of a method's bytecode that format version 3 records
/// (`code_hash`) and the seeding doors compare. FNV-1a, spelled out here
/// rather than taken from a hasher crate, so two builds of the VM agree on it
/// whatever their dependency versions. Never `0`, which means "unknown".
pub fn bytecode_hash(code: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in code {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// What the VM says about a replayed method's bytecode in THIS run: the answer
/// of the second resolver both seeding doors take, `(class id, method name,
/// descriptor) -> LiveCode` (r11-tier-profile-replay-residuals item 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveCode {
    /// The class declares the method, with a Code attribute of this many bytes;
    /// its hash is not known, so only the length is checked.
    Len(usize),
    /// The class declares the method, with a Code attribute of `len` bytes
    /// whose [`bytecode_hash`] is `hash`. What the VM's resolver answers
    /// (format version 3).
    Code { len: usize, hash: u64 },
    /// The class declares no such method, or the method has no bytecode.
    Absent,
    /// The VM could not look without blocking. The record keeps waiting and
    /// is asked again at the next definition sweep.
    Busy,
}

/// What a seeding door does with one record whose class resolved.
enum CodeCheck {
    /// Seed it; the live method is this many bytes.
    Seed(usize),
    /// Drop it and count it in [`LoadCensus::methods_stale_code`].
    Stale,
    /// Leave it waiting ([`LiveCode::Busy`]).
    Wait,
}

/// Match `record` against the live method. See the module doc's "Stale
/// records" for the rule.
fn check_live_code(record: &MethodRecord, live: LiveCode) -> CodeCheck {
    let (len, hash) = match live {
        LiveCode::Len(len) => (len, None),
        LiveCode::Code { len, hash } => (len, Some(hash)),
        LiveCode::Absent => return CodeCheck::Stale,
        LiveCode::Busy => return CodeCheck::Wait,
    };
    if record.code_len != 0 && usize::try_from(record.code_len).map_or(true, |saved| saved != len) {
        return CodeCheck::Stale;
    }
    // Version 3: the same length with other bytes (a changed constant, a
    // swapped operand) is another method too. `0` on either side is unknown.
    if record.code_hash != 0 && hash.is_some_and(|live| live != 0 && live != record.code_hash) {
        return CodeCheck::Stale;
    }
    // Also for a record whose saving run did not know the length (0): a pc
    // past the live code cannot describe this method.
    let in_code = |pc: u32| usize::try_from(pc).map_or(false, |pc| pc < len);
    let every_pc_in_code = record.branches.iter().all(|b| in_code(b.0))
        && record.call_sites.iter().all(|c| in_code(c.0))
        && record.loops.iter().all(|l| in_code(l.0))
        && record.receivers.iter().all(|r| in_code(r.0));
    if every_pc_in_code {
        CodeCheck::Seed(len)
    } else {
        CodeCheck::Stale
    }
}

/// Seed `store` from `bytes`, resolving every class name through `class_id`
/// and checking every record against the live bytecode through `code_len`.
///
/// Uses [`ReplaySeedPolicy::from_env`]. Every seeded number is a **hint**: see
/// that type for the per-axis rule and the argument for it, and the module doc
/// for what an adversarial file can and cannot cause.
pub fn load_into(
    store: &ProfileStore,
    bytes: &[u8],
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> Result<LoadCensus, ProfileStoreError> {
    load_into_with_policy(
        store,
        bytes,
        class_id,
        code_len,
        &ReplaySeedPolicy::from_env(),
    )
}

/// [`load_into`] with an explicit seeding policy.
///
/// Separate from [`load_into`] so tests can pin the policy instead of inheriting
/// whatever the ambient environment says — a test whose expected numbers depend
/// on `CRATONVM_JIT_THRESHOLD` passes or fails for a reason unrelated to what it
/// claims to check.
pub fn load_into_with_policy(
    store: &ProfileStore,
    bytes: &[u8],
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
    policy: &ReplaySeedPolicy,
) -> Result<LoadCensus, ProfileStoreError> {
    let records = decode(bytes)?;
    let mut census = LoadCensus {
        methods_in_file: records.len().min(u32::MAX as usize) as u32,
        ..LoadCensus::default()
    };

    // One resolver call per distinct name. The resolver the VM passes takes a
    // class-manager lock, and a receiver class name repeats at every site that
    // sees it.
    let mut resolved: FxHashMap<String, Option<u32>> = FxHashMap::default();
    let mut resolve = |name: &str| -> Option<u32> {
        if let Some(cached) = resolved.get(name).copied() {
            return cached;
        }
        let id = class_id(name);
        resolved.insert(name.to_string(), id);
        id
    };

    // Records whose class is not defined YET. The VM loads the file in
    // `SharedVm::new`, before any Java runs, when only bootstrap classes
    // exist -- so this is nearly the whole application profile, not "classes
    // this run never loads". They wait, by class name, for the definition
    // hook ([`seed_pending_replay_for_class`]).
    let mut deferred: FxHashMap<String, Vec<MethodRecord>> = FxHashMap::default();

    for record in records {
        let Some(owner) = resolve(record.class_name.as_str()) else {
            census.methods_unresolved_class = census.methods_unresolved_class.saturating_add(1);
            deferred
                .entry(record.class_name.clone())
                .or_default()
                .push(record);
            continue;
        };
        let live = code_len(
            owner,
            record.method_name.as_str(),
            record.descriptor.as_str(),
        );
        match check_live_code(&record, live) {
            CodeCheck::Seed(len) => {
                seed_record(
                    store,
                    &record,
                    owner,
                    len,
                    &mut resolve,
                    policy,
                    &mut census,
                );
            }
            CodeCheck::Stale => {
                census.methods_stale_code = census.methods_stale_code.saturating_add(1);
            }
            // Parked like an undefined class's record. The VM's definition
            // sweep starts at class slot 0, so it revisits this class.
            CodeCheck::Wait => {
                census.methods_unresolved_class = census.methods_unresolved_class.saturating_add(1);
                deferred
                    .entry(record.class_name.clone())
                    .or_default()
                    .push(record);
            }
        }
    }

    if !deferred.is_empty() {
        store.replay_pending.defer(deferred, *policy);
    }

    Ok(census)
}

/// `value` from a group of counts whose largest is `max`, scaled by the one
/// factor `ceiling / max` that brings `max` down to `ceiling` — so the group
/// keeps its RATIOS, which are the hint, while its magnitude is capped.
///
/// A group already at or under the ceiling is returned unchanged. A non-zero
/// count never scales to zero: a receiver class that vanished from the seed
/// would stop being "named by the replay", and its first live observation
/// would then refute the site's other seeds.
fn scale_to_ceiling(value: u32, max: u32, ceiling: u32) -> u32 {
    if max <= ceiling || value == 0 {
        return value.min(ceiling);
    }
    let scaled = u64::from(value) * u64::from(ceiling) / u64::from(max);
    u32::try_from(scaled).unwrap_or(ceiling).max(1)
}

/// Seed one decoded record whose class resolved to `owner` and whose live
/// method ([`check_live_code`]) is `live_len` bytes, resolving receiver class
/// names through `resolve`, and account for it in `census`.
///
/// Every receiver name is resolved BEFORE the method's profile `Mutex` is
/// taken: the resolver reaches into the VM's class manager, and holding a
/// profile slot across that is exactly the lock inversion `snapshot_all`'s
/// two-phase pattern exists to avoid.
fn seed_record(
    store: &ProfileStore,
    record: &MethodRecord,
    owner: u32,
    live_len: usize,
    resolve: &mut dyn FnMut(&str) -> Option<u32>,
    policy: &ReplaySeedPolicy,
    census: &mut LoadCensus,
) {
    let key = MethodKey {
        class_id: owner,
        method_name: std::sync::Arc::from(record.method_name.as_str()),
        descriptor: std::sync::Arc::from(record.descriptor.as_str()),
    };

    let mut receiver_seeds: Vec<(usize, Vec<(u32, u32)>)> = Vec::new();
    for (pc, table) in &record.receivers {
        let mut ids: Vec<(u32, u32)> = Vec::new();
        // The whole site scales by one factor (r11-tier). Capping each class
        // on its own turned `{A: 1e6, B: 5 000}` into `{1 000, 1 000}` — a
        // monomorphic site replayed as a 50/50 bimorphic one.
        let site_max = table.iter().map(|(_, c)| *c).max().unwrap_or(0);
        for (name, count) in table {
            match resolve(name.as_str()) {
                Some(id) => ids.push((
                    id,
                    scale_to_ceiling(*count, site_max, policy.receiver_ceiling),
                )),
                None => {
                    census.receiver_types_unresolved =
                        census.receiver_types_unresolved.saturating_add(1);
                }
            }
        }
        // One seed per class. A file that names a class twice at one site (a
        // writer before round 12 wave 1 did, for two loaders' classes of one
        // name; a hand-made file can repeat it N times) would otherwise seed
        // it additively, past `receiver_ceiling`. The larger entry stands for
        // both, as a repeated method key keeps one record.
        ids.sort_unstable_by_key(|&(id, _)| id);
        ids.dedup_by(|later, kept| {
            if later.0 == kept.0 {
                kept.1 = kept.1.max(later.1);
                true
            } else {
                false
            }
        });
        if !ids.is_empty() {
            receiver_seeds.push((*pc as usize, ids));
        }
    }

    let seeded_invocations = policy.seed_invocations(record.invocations);
    let branch_sites = record.branches.len().min(u32::MAX as usize) as u32;
    let receiver_pairs = receiver_seeds.iter().fold(0u32, |acc, (_, v)| {
        acc.saturating_add(v.len().min(u32::MAX as usize) as u32)
    });

    store.with_method_profile_mut(&key, |profile| {
        // The live length, so a save of a seeded method that this run never
        // stamped (no fast-path branch) still writes a checkable record.
        profile.note_code_len(live_len);
        for &(pc, taken, not_taken) in &record.branches {
            // Both directions by one factor, like the loop pair below: capping
            // each on its own seeded `1e6 / 200` as a neutral `100 / 100`.
            let site_max = taken.max(not_taken);
            profile.seed_replayed_branch(
                pc as usize,
                scale_to_ceiling(taken, site_max, policy.branch_ceiling),
                scale_to_ceiling(not_taken, site_max, policy.branch_ceiling),
            );
        }
        for &(pc, count) in &record.call_sites {
            profile.seed_replayed_call_site(pc as usize, count.min(policy.call_site_ceiling));
        }
        for &(pc, backedge, entries, trips) in &record.loops {
            let capped_entries = entries.min(policy.loop_entry_ceiling);
            // Scale `total_trips` with `entry_count` so the AVERAGE — the
            // only thing `suggests_unroll_factor` reads — is preserved. In
            // `u128` because `trips * capped` overflows `u64` for a loop
            // that ran for a long time.
            let scaled_trips = if entries == 0 || capped_entries == entries {
                trips
            } else {
                let scaled =
                    u128::from(trips) * u128::from(capped_entries) / u128::from(entries).max(1);
                scaled.min(u128::from(u64::MAX)) as u64
            };
            profile.seed_replayed_loop(
                pc as usize,
                backedge.min(policy.backedge_ceiling),
                capped_entries,
                scaled_trips,
            );
        }
        for (pc, ids) in &receiver_seeds {
            for &(receiver_id, count) in ids {
                profile.seed_replayed_receiver(*pc, receiver_id, count);
            }
        }
        if seeded_invocations > 0 {
            profile.note_replayed_invocations(seeded_invocations);
        }
    });

    if seeded_invocations > 0 {
        let packed = cratonvm_jit_api::invoc_key_parts(
            owner,
            record.method_name.as_str(),
            record.descriptor.as_str(),
        );
        store.add_invocations(packed, seeded_invocations);
    }

    census.methods_seeded = census.methods_seeded.saturating_add(1);
    census.branch_sites_seeded = census.branch_sites_seeded.saturating_add(branch_sites);
    census.receiver_types_replayed = census
        .receiver_types_replayed
        .saturating_add(receiver_pairs);
    census.invocations_seeded = census
        .invocations_seeded
        .saturating_add(u64::from(seeded_invocations));
}

// ---------------------------------------------------------------------------
// Deferred replay: seeding at class definition
// ---------------------------------------------------------------------------

/// Replay records waiting for their class to be defined, per [`ProfileStore`].
///
/// `CRATONVM_JIT_PROFILE_LOAD` is read in `SharedVm::new`, before any Java
/// executes, when the class manager holds only what VM bootstrap defined. Every
/// application class -- and every JDK class loaded lazily later -- was
/// unresolvable then, so the replay used to seed a handful of bootstrap classes
/// and silently drop the application profile as "a class this run never
/// loaded" (`docs/known-issues/jit/profile-replay-resolves-classes-before-the-application-loads-them-20260918.md`).
/// [`load_into_with_policy`] now parks those records here, keyed by class
/// name, and the VM's class-definition hook hands each newly defined name to
/// [`seed_pending_replay_for_class`], which seeds them through the same path
/// an eager seed takes. A class definition precedes every execution of the
/// class's methods, so the seeds still land before the first frame of the
/// method they describe runs -- which is the property the eager load had
/// only for bootstrap classes.
///
/// Seeding at definition still loads nothing: the hook reacts to a
/// definition the program made, so a profile file still cannot change what
/// the program does.
#[derive(Default)]
pub struct ReplayPending {
    /// Class names with records waiting: the definition hook's lock-free
    /// "nothing to do" test, so a run without a replay (or one whose replay
    /// has fully landed) pays one relaxed load per definition.
    classes: std::sync::atomic::AtomicUsize,
    inner: parking_lot::Mutex<ReplayPendingInner>,
    /// Methods seeded by the definition hook, over the life of the store.
    seeded_on_definition: std::sync::atomic::AtomicU32,
    /// Receiver pairs the definition hook dropped because the RECEIVER class
    /// was still undefined when the owning class was. The smaller loss (see
    /// the page's proposal 3); counted so it is visible.
    receivers_unresolved_on_definition: std::sync::atomic::AtomicU32,
    /// Methods the definition hook dropped as stale ([`check_live_code`]).
    stale_on_definition: std::sync::atomic::AtomicU32,
}

#[derive(Default)]
struct ReplayPendingInner {
    by_class: FxHashMap<String, Vec<MethodRecord>>,
    /// The policy the deferring load used; every pending record is seeded
    /// under it. A second load replaces it (and merges its records).
    policy: Option<ReplaySeedPolicy>,
}

/// Class names waiting in ANY store in the process: the VM's class-definition
/// hook runs on every class-manager write-guard release, before it has
/// picked a VM, so its "nothing to do" test has to be process-wide. See
/// [`any_replay_pending`].
static REPLAY_PENDING_CLASSES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Whether any profile store in the process has replay records waiting for a
/// class definition. One relaxed load; `false` in every run that does not
/// replay a profile, and again once every replayed class has been defined.
#[inline]
pub fn any_replay_pending() -> bool {
    REPLAY_PENDING_CLASSES.load(std::sync::atomic::Ordering::Relaxed) != 0
}

impl ReplayPending {
    fn defer(&self, records: FxHashMap<String, Vec<MethodRecord>>, policy: ReplaySeedPolicy) {
        let mut inner = self.inner.lock();
        let before = inner.by_class.len();
        for (class_name, mut list) in records {
            inner
                .by_class
                .entry(class_name)
                .or_default()
                .append(&mut list);
        }
        inner.policy = Some(policy);
        let after = inner.by_class.len();
        self.classes
            .store(after, std::sync::atomic::Ordering::Release);
        REPLAY_PENDING_CLASSES.fetch_add(after - before, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether any record is waiting for `class_name`.
    fn has(&self, class_name: &str) -> bool {
        if self.classes.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return false;
        }
        self.inner.lock().by_class.contains_key(class_name)
    }

    /// Take every record waiting for `class_name`, with the policy to seed it
    /// under.
    fn take(&self, class_name: &str) -> Option<(Vec<MethodRecord>, ReplaySeedPolicy)> {
        let mut inner = self.inner.lock();
        let policy = inner.policy?;
        let records = inner.by_class.remove(class_name)?;
        self.classes
            .store(inner.by_class.len(), std::sync::atomic::Ordering::Release);
        REPLAY_PENDING_CLASSES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        Some((records, policy))
    }
}

impl Drop for ReplayPending {
    /// A store dropped with records still waiting (a VM torn down, a test's
    /// store) takes its share of the process-wide count with it.
    fn drop(&mut self) {
        let waiting = self.inner.get_mut().by_class.len();
        REPLAY_PENDING_CLASSES.fetch_sub(waiting, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Seed the replay records waiting for `class_name`, which the VM has just
/// defined. Returns how many methods were seeded.
///
/// The VM's class-definition hook calls this once per defined class, after
/// the class manager's write guard is released (the resolver takes a read
/// lock). `class_id` is the same resolver [`load_if_configured`] takes --
/// `find_unique_class_by_name` -- and it is asked again here rather than
/// trusting the definition, so a name two loaders have defined (ambiguous)
/// seeds nothing, exactly as an eager load would have refused it. An
/// unresolved answer leaves the records waiting. `code_len` checks each record
/// against the live bytecode, as the eager load does: a stale record is
/// dropped and counted, and a [`LiveCode::Busy`] one is put back to wait.
///
/// Cost with nothing pending: one relaxed-acquire load.
pub fn seed_pending_replay_for_class(
    store: &ProfileStore,
    class_name: &str,
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> u32 {
    let pending = &store.replay_pending;
    if !pending.has(class_name) {
        return 0;
    }
    // Resolve with no lock of ours held: the resolver takes the class manager.
    let Some(owner) = class_id(class_name) else {
        return 0;
    };
    let Some((records, policy)) = pending.take(class_name) else {
        // Another thread's hook took them first.
        return 0;
    };
    let mut resolved: FxHashMap<String, Option<u32>> = FxHashMap::default();
    let mut resolve = |name: &str| -> Option<u32> {
        if let Some(cached) = resolved.get(name).copied() {
            return cached;
        }
        let id = class_id(name);
        resolved.insert(name.to_string(), id);
        id
    };
    let mut census = LoadCensus::default();
    let mut waiting: Vec<MethodRecord> = Vec::new();
    for record in records {
        let live = code_len(
            owner,
            record.method_name.as_str(),
            record.descriptor.as_str(),
        );
        match check_live_code(&record, live) {
            CodeCheck::Seed(len) => {
                seed_record(
                    store,
                    &record,
                    owner,
                    len,
                    &mut resolve,
                    &policy,
                    &mut census,
                );
            }
            CodeCheck::Stale => {
                census.methods_stale_code = census.methods_stale_code.saturating_add(1);
            }
            CodeCheck::Wait => waiting.push(record),
        }
    }
    if !waiting.is_empty() {
        let mut back: FxHashMap<String, Vec<MethodRecord>> = FxHashMap::default();
        back.insert(class_name.to_string(), waiting);
        pending.defer(back, policy);
    }
    pending
        .seeded_on_definition
        .fetch_add(census.methods_seeded, std::sync::atomic::Ordering::Relaxed);
    pending.receivers_unresolved_on_definition.fetch_add(
        census.receiver_types_unresolved,
        std::sync::atomic::Ordering::Relaxed,
    );
    pending.stale_on_definition.fetch_add(
        census.methods_stale_code,
        std::sync::atomic::Ordering::Relaxed,
    );
    census.methods_seeded
}

/// Seed every waiting record whose class resolves now. Returns how many
/// methods were seeded.
///
/// For a VM hook that knows a class was defined but not which one. Resolves
/// every pending class name, so prefer [`seed_pending_replay_for_class`]; a
/// caller of this one should gate it on the class-definition epoch having
/// moved.
pub fn seed_pending_replay(
    store: &ProfileStore,
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> u32 {
    if store
        .replay_pending
        .classes
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        return 0;
    }
    let names: Vec<String> = store
        .replay_pending
        .inner
        .lock()
        .by_class
        .keys()
        .cloned()
        .collect();
    let mut seeded = 0u32;
    for name in names {
        seeded = seeded.saturating_add(seed_pending_replay_for_class(
            store, &name, class_id, code_len,
        ));
    }
    seeded
}

/// `(class names still waiting, methods still waiting, methods seeded at
/// definition, receiver pairs dropped at definition, methods dropped as stale
/// at definition)` for `store`'s replay.
///
/// What is still waiting at exit is the part of the profile this run never
/// defined a class for (or defined ambiguously) -- the number the eager load
/// used to report, at the wrong time, as `methods_unresolved_class`.
pub fn replay_pending_census(store: &ProfileStore) -> (usize, usize, u32, u32, u32) {
    let pending = &store.replay_pending;
    let (classes, methods) = {
        let inner = pending.inner.lock();
        (
            inner.by_class.len(),
            inner.by_class.values().map(Vec::len).sum::<usize>(),
        )
    };
    (
        classes,
        methods,
        pending
            .seeded_on_definition
            .load(std::sync::atomic::Ordering::Relaxed),
        pending
            .receivers_unresolved_on_definition
            .load(std::sync::atomic::Ordering::Relaxed),
        pending
            .stale_on_definition
            .load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Sum the per-method replay provenance across the whole store.
///
/// See [`ReplayOutcome`] for what the numbers mean and what the walk cannot
/// see.
pub fn replay_outcome(store: &ProfileStore) -> ReplayOutcome {
    let mut out = ReplayOutcome::default();
    for (_, profile) in store.snapshot_all() {
        let Some(replay) = profile.replay_provenance() else {
            continue;
        };
        out.methods_seeded = out.methods_seeded.saturating_add(1);
        if replay.shape_contradicted {
            out.methods_shape_contradicted = out.methods_shape_contradicted.saturating_add(1);
        }
        out.receiver_types_replayed = out
            .receiver_types_replayed
            .saturating_add(replay.seeded_receiver_types);
        out.receiver_types_confirmed = out
            .receiver_types_confirmed
            .saturating_add(replay.receivers_confirmed);
        out.receiver_types_refuted = out
            .receiver_types_refuted
            .saturating_add(replay.receivers_refuted);
        out.receiver_types_unjudged = out
            .receiver_types_unjudged
            .saturating_add(replay.receivers_unjudged());
        out.invocations_seeded = out
            .invocations_seeded
            .saturating_add(u64::from(replay.seeded_invocations));
    }
    out
}

/// Emit the replay outcome census.
///
/// `info!` and `warn!`, never `debug!` or `trace!`. The workspace `Cargo.toml`
/// pins `tracing`'s `release_max_level_info`, which expands `debug!` and
/// `trace!` to no-ops in a release build — so a counter whose only reader is a
/// `debug!` cannot be recovered with any `RUST_LOG` value on a released binary,
/// and is therefore not an instrument at all. That corollary is written out in
/// the `tracing` dependency comment in the workspace manifest, and these are
/// exactly the numbers it is about: whether replay helped is a question only a
/// real run can answer, and a real run is a release build.
///
/// The refuted line is a `warn!` because a refuted seed is not neutral — it
/// bought a speculation that the live run then had to undo.
pub fn report_replay_outcome(store: &ProfileStore) {
    let (pending_classes, pending_methods, seeded_later, receivers_dropped_later, stale_later) =
        replay_pending_census(store);
    if pending_classes > 0 || seeded_later > 0 || stale_later > 0 {
        tracing::info!(
            "[jit-profile-replay] {} method(s) seeded when their class was defined \
             ({} receiver type(s) named a class not yet defined then); {} dropped as stale \
             (bytecode length or method changed since the save); {} method(s) in {} \
             class(es) still waiting -- never defined this run, or defined by more than one \
             loader",
            seeded_later,
            receivers_dropped_later,
            stale_later,
            pending_methods,
            pending_classes,
        );
    }
    let outcome = replay_outcome(store);
    if outcome.methods_seeded == 0 {
        return;
    }
    tracing::info!(
        "[jit-profile-replay] seeded {} method(s); receiver types: {} replayed, {} confirmed, \
         {} refuted, {} unjudged; {} invocation(s) of credit came from the replay",
        outcome.methods_seeded,
        outcome.receiver_types_replayed,
        outcome.receiver_types_confirmed,
        outcome.receiver_types_refuted,
        outcome.receiver_types_unjudged,
        outcome.invocations_seeded,
    );
    if outcome.receiver_types_refuted > 0 {
        tracing::warn!(
            "[jit-profile-replay] {} method(s) had a call site whose replayed receiver shape was \
             contradicted by live execution ({} receiver type(s) discarded). Those seeds bought \
             a speculation the live run then had to correct; a profile whose refuted count \
             approaches its confirmed count is stale enough to be costing more than it saves.",
            outcome.methods_shape_contradicted,
            outcome.receiver_types_refuted,
        );
    }
}

// ---------------------------------------------------------------------------
// Files and flags
// ---------------------------------------------------------------------------

/// The path a declared path-valued flag names, or `None` for off.
///
/// Read for its **value**, never its presence: these flags take a path, so
/// `..._SAVE=` with an empty value is off rather than "save somewhere". An
/// all-whitespace value is off for the same reason — it is what a shell script
/// produces from an unset variable it meant to substitute.
fn configured_path(var: &str) -> Option<PathBuf> {
    let value = cratonvm_types::flags::runtime_var(var).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// The path `CRATONVM_JIT_PROFILE_SAVE` names, or `None` when saving is off.
pub fn save_path_from_env() -> Option<PathBuf> {
    configured_path(SAVE_PATH_VAR)
}

/// The path `CRATONVM_JIT_PROFILE_LOAD` names, or `None` when loading is off.
pub fn load_path_from_env() -> Option<PathBuf> {
    configured_path(LOAD_PATH_VAR)
}

/// Write the live store to `path`.
///
/// # Not written atomically, and why that is the right trade
///
/// This writes `path` directly rather than writing a sibling temporary and
/// renaming over it. A process killed mid-write therefore leaves a truncated
/// file. That is survivable *by construction*: [`load_from_path`] rejects a
/// truncation at **every** prefix length — which is a property this module
/// tests exhaustively rather than assumes — and the VM then starts with an
/// empty profile, which is exactly where it would have started without the
/// file. Adding a rename would buy one retained older profile in exchange for a
/// second failure mode (a temporary left behind when the rename fails) on a
/// path that runs during shutdown. If a future change makes a stale profile
/// more valuable than a missing one, the rename is the edit, and the loader
/// needs no change for it.
///
/// # Except against a second writer (round 13 wave 3, lane replay2)
///
/// The argument above covers ONE process. Two VMs (two processes) saving to the
/// same `CRATONVM_JIT_PROFILE_SAVE` path at exit truncate and write one file
/// concurrently, and what is left need not be either run's encoding; the
/// decoder refuses nearly every such splice, but one landing on record
/// boundaries passes it. So the bytes go to a sibling `<name>.tmp.<pid>` and
/// are renamed over `path` (`std::fs::rename` replaces an existing file on
/// every platform this VM runs on): each reader sees one writer's whole file.
/// A failed rename (a reader holding the file open on Windows, a cross-device
/// path) removes the temporary and falls back to the direct write, so the
/// failure mode above is the worst case, never a new one. Kill switch
/// `CRATONVM_JIT_PROFILE_SAVE_ATOMIC=0` (proposal R13-6).
///
/// `live_code` answers the SAVING run's live method for each record (the
/// module doc's "Exact records"); a resolver that answers
/// [`LiveCode::Busy`] writes every record with the length its recorders
/// stamped and no hash.
pub fn save_to_path(
    store: &ProfileStore,
    path: &Path,
    class_name: &dyn Fn(u32) -> Option<String>,
    live_code: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> Result<SaveCensus, ProfileStoreError> {
    let (bytes, census) = serialize_against_live_code(store, class_name, live_code);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_PROFILE_SAVE_ATOMIC")
        && write_whole_file_by_rename(path, &bytes)
    {
        return Ok(census);
    }
    std::fs::write(path, &bytes)?;
    Ok(census)
}

/// Write `bytes` to a sibling temporary of `path` and rename it over `path`
/// ([`save_to_path`]'s second-writer section). `false`, with no temporary left
/// behind, when any step fails: the caller then writes `path` directly.
fn write_whole_file_by_rename(path: &Path, bytes: &[u8]) -> bool {
    let Some(tmp) = temporary_sibling(path) else {
        return false;
    };
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// The temporary [`write_whole_file_by_rename`] writes before its rename:
/// `<name>.tmp.<pid>.<thread>`, `None` for a path with no file name.
///
/// Round 13 wave 9 (lane replay5): the name was `<name>.tmp.<pid>`, which is
/// one name for every VM of a process. Two embedded VMs dropped on two
/// threads at once (each `impl Drop for Vm` saves) wrote the SAME temporary
/// concurrently, and whichever renamed first could publish the other's
/// half-written bytes: the very splice the rename exists to rule out. A save
/// runs on the thread that drops its VM, and one thread saves one store at a
/// time, so the thread's identity separates every concurrent writer.
fn temporary_sibling(path: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let name = path.file_name()?;
    let mut thread = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut thread);
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(
        ".tmp.{}.{:016x}",
        std::process::id(),
        thread.finish()
    ));
    Some(path.with_file_name(tmp_name))
}

/// Read `path` into the live store.
///
/// # A missing file is not an error
///
/// A path that does not exist returns `Ok` with [`LoadCensus::file_missing`]
/// set. That is the first run of a save/load pair, and it is the same outcome
/// as an empty profile: **no hints**. Making it an `Err` would put a scary line
/// in front of every first run of a two-run measurement and would tempt the
/// call site into ignoring errors wholesale — including the ones that mean the
/// file is corrupt, which are the ones worth seeing.
///
/// Anything else — an unreadable file, a bad magic, a truncation — is an `Err`,
/// and the store is left exactly as it was found. The WHOLE file is decoded
/// ([`decode`]) before the first seed is written ([`load_into_with_policy`]),
/// so a file that fails half way through seeds nothing at all: loading is
/// all-or-nothing. (This paragraph used to say the opposite — that seeding
/// happened as records were decoded and a failure left the first half seeded
/// — which described no version of the code below.) [`load_if_configured`]
/// reports the failure.
pub fn load_from_path(
    store: &ProfileStore,
    path: &Path,
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> Result<LoadCensus, ProfileStoreError> {
    let bytes = match read_bounded_profile_file(path) {
        Ok(bytes) => bytes,
        Err(ProfileStoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadCensus {
                file_missing: true,
                ..LoadCensus::default()
            });
        }
        Err(e) => return Err(e),
    };
    load_into(store, &bytes, class_id, code_len)
}

/// The largest profile file [`load_from_path`] will read.
///
/// Far above anything `serialize` produces for a real program (a record is a
/// few hundred bytes), and far below what would hurt at VM start.
const MAX_PROFILE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Read `path` as a profile file: a REGULAR file of at most
/// [`MAX_PROFILE_FILE_BYTES`].
///
/// r11-tier: this was `std::fs::read`, which reads to end-of-file — and the
/// module's own hostile-input example, `/dev/urandom`, has none, as has
/// `/dev/zero`; a FIFO blocks. VM start would hang with memory growing without
/// bound, against the module doc's promise of "never a large allocation". The
/// `take` below also bounds a file that grows while it is read.
fn read_bounded_profile_file(path: &Path) -> Result<Vec<u8>, ProfileStoreError> {
    use std::io::Read;
    // Asked of the PATH first: Windows refuses to `open` a directory with an
    // error that is not a refusal of the kind below, and a FIFO would block in
    // `open` itself. The handle is asked again after opening, for a path that
    // was swapped in between.
    if !std::fs::metadata(path)?.is_file() {
        return format_err(format!(
            "{} is not a regular file; a profile is only read from one",
            path.display()
        ));
    }
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return format_err(format!(
            "{} is not a regular file; a profile is only read from one",
            path.display()
        ));
    }
    if meta.len() > MAX_PROFILE_FILE_BYTES {
        return format_err(format!(
            "{} is {} bytes, above the {MAX_PROFILE_FILE_BYTES}-byte profile file limit",
            path.display(),
            meta.len()
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROFILE_FILE_BYTES {
        return format_err(format!(
            "{} grew past the {MAX_PROFILE_FILE_BYTES}-byte profile file limit while it was read",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Save the store if `CRATONVM_JIT_PROFILE_SAVE` names a path. Never fails.
///
/// Returns whether a file was written. Errors become a `warn!`, because failing
/// a VM's shutdown over a profile file would be the same category error the
/// module doc forbids: a profile is an optimization hint, and a hint must not
/// be able to change what the program does — including by turning a successful
/// run into a failed one.
pub fn save_if_configured(
    store: &ProfileStore,
    class_name: &dyn Fn(u32) -> Option<String>,
    live_code: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> bool {
    let Some(path) = save_path_from_env() else {
        return false;
    };
    match save_to_path(store, &path, class_name, live_code) {
        Ok(census) => {
            tracing::info!(
                "[jit-profile-save] wrote {} byte(s) to {}: {} of {} method(s) \
                 ({} skipped for an unnameable class, {} with no evidence), \
                 {} receiver type(s) ({} dropped over the per-site cap, {} unnameable)",
                census.bytes,
                path.display(),
                census.methods_written,
                census.methods_in_store,
                census.methods_skipped_unnamed,
                census.methods_skipped_empty,
                census.receiver_types_written,
                census.receiver_types_dropped_over_cap,
                census.receiver_types_unnamed,
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                "[jit-profile-save] could not write {}: {e}. The run is unaffected; the next \
                 one simply starts without hints.",
                path.display(),
            );
            false
        }
    }
}

/// Load the store if `CRATONVM_JIT_PROFILE_LOAD` names a path. Never fails.
///
/// Returns whether anything was seeded. A corrupt or truncated file becomes a
/// `warn!` and the VM continues with an empty profile, which is the whole
/// contract: **a bad profile must never be able to change a program's answer**,
/// and refusing to start is a way of changing it.
pub fn load_if_configured(
    store: &ProfileStore,
    class_id: &dyn Fn(&str) -> Option<u32>,
    code_len: &dyn Fn(u32, &str, &str) -> LiveCode,
) -> bool {
    let Some(path) = load_path_from_env() else {
        return false;
    };
    match load_from_path(store, &path, class_id, code_len) {
        Ok(census) if census.file_missing => {
            tracing::info!(
                "[jit-profile-load] {} does not exist yet; starting with no hints. This is the \
                 expected first run of a save/load pair.",
                path.display(),
            );
            false
        }
        Ok(census) => {
            tracing::info!(
                "[jit-profile-load] {}: seeded {} of {} method(s) now ({} wait for their class \
                 to be defined, {} dropped as stale), {} branch site(s), {} receiver type(s) \
                 ({} named an unloaded class), {} invocation(s) of credit after the seed \
                 policy's divisor and ceiling",
                path.display(),
                census.methods_seeded,
                census.methods_in_file,
                census.methods_unresolved_class,
                census.methods_stale_code,
                census.branch_sites_seeded,
                census.receiver_types_replayed,
                census.receiver_types_unresolved,
                census.invocations_seeded,
            );
            census.methods_seeded > 0 || census.methods_unresolved_class > 0
        }
        Err(e) => {
            tracing::warn!(
                "[jit-profile-load] ignoring {}: {e}. The VM continues with an empty profile — \
                 a profile is an optimization hint, and a bad one must never change what the \
                 program does.",
                path.display(),
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── fixtures ────────────────────────────────────────────────────────────

    /// A five-class name space. [`namer`] and [`resolver`] are exact inverses
    /// over it — which is what a real VM's class manager gives us, and what the
    /// identity-by-name property is stated against.
    const CLASSES: &[&str] = &[
        "java/lang/Object",
        "java/util/HashMap",
        "org/example/Service",
        "org/example/Impl",
        "org/example/Other",
    ];

    /// Class name -> id, with `base` as the id of `CLASSES[0]`. Two different
    /// `base` values model two different runs of the same program.
    fn namer(base: u32) -> impl Fn(u32) -> Option<String> {
        move |id: u32| {
            let index = id.checked_sub(base)? as usize;
            CLASSES.get(index).map(|s| (*s).to_string())
        }
    }

    fn resolver(base: u32) -> impl Fn(&str) -> Option<u32> {
        move |name: &str| {
            CLASSES
                .iter()
                .position(|c| *c == name)
                .map(|i| base + i as u32)
        }
    }

    /// The live-bytecode resolver the tests seed against: `handle` is 64 bytes
    /// and `run` 8, the lengths [`populated`] stamps; every other method is 256
    /// bytes, past every pc the tests record.
    fn live_len(_class_id: u32, name: &str, _descriptor: &str) -> LiveCode {
        match name {
            "handle" => LiveCode::Len(64),
            "run" => LiveCode::Len(8),
            _ => LiveCode::Len(256),
        }
    }

    fn key(class_id: u32, name: &str, descriptor: &str) -> MethodKey {
        MethodKey {
            class_id,
            method_name: Arc::from(name),
            descriptor: Arc::from(descriptor),
        }
    }

    /// A store with every map populated, built at class-id base `base`.
    fn populated(base: u32) -> ProfileStore {
        let store = ProfileStore::new();
        let service = key(base + 2, "handle", "(Ljava/lang/Object;)V");
        store.with_method_profile_mut(&service, |p| {
            p.note_code_len(64);
            p.record_branch(7, true);
            p.record_branch(7, true);
            p.record_branch(7, false);
            p.record_branch(19, false);
            p.record_call_site(23);
            p.record_call_site(23);
            p.record_backedge(31);
            p.record_trip_complete(31, 12);
            // Receivers: `Impl` dominant, `Other` in the tail.
            for _ in 0..40 {
                p.record_receiver(11, base + 3);
            }
            for _ in 0..3 {
                p.record_receiver(11, base + 4);
            }
            p.record_receiver(45, base + 1);
        });
        store.add_invocations(
            cratonvm_jit_api::invoc_key_parts(base + 2, "handle", "(Ljava/lang/Object;)V"),
            888,
        );

        let helper = key(base + 3, "run", "()I");
        store.with_method_profile_mut(&helper, |p| {
            p.note_code_len(8);
            p.record_branch(2, false);
            p.record_call_site(5);
        });
        store
    }

    /// The store's content as the file describes it, so two stores can be
    /// compared without depending on `class_id` at all.
    fn view(store: &ProfileStore, base: u32) -> Vec<MethodRecord> {
        let mut census = SaveCensus::default();
        collect_records(store, &namer(base), &no_live_code, &mut census, true)
    }

    /// `view` with the invocation counts blanked. The round trip deliberately
    /// does NOT preserve invocation magnitude — `ReplaySeedPolicy` divides and
    /// caps it — so the maps are compared here and the counter separately.
    fn view_without_invocations(store: &ProfileStore, base: u32) -> Vec<MethodRecord> {
        let mut records = view(store, base);
        for r in &mut records {
            r.invocations = 0;
        }
        records
    }

    /// A well-formed file from [`populated`]: two methods, between them at
    /// least one entry in every array the format has, so the prefix walk in
    /// [`every_truncation_is_an_error`] exercises every count.
    fn one_of_everything() -> Vec<u8> {
        let store = populated(100);
        let (bytes, _) = serialize(&store, &namer(100));
        bytes
    }

    // ── round trip ──────────────────────────────────────────────────────────

    /// A populated store serialises and deserialises to an equal store.
    ///
    /// Equality is taken on the *file view* rather than on the in-memory store,
    /// because the in-memory store is keyed by `class_id` and the whole point
    /// of the format is that `class_id` does not survive a process. Two stores
    /// that produce the same bytes hold the same profile.
    #[test]
    fn a_populated_store_round_trips() {
        let source = populated(100);
        let (bytes, save) = serialize(&source, &namer(100));
        assert_eq!(save.methods_written, 2);
        assert!(save.receiver_types_written >= 3);

        let restored = ProfileStore::new();
        let census = match load_into_with_policy(
            &restored,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        ) {
            Ok(c) => c,
            Err(e) => {
                assert!(false, "round trip failed to load: {e}");
                return;
            }
        };
        assert_eq!(census.methods_in_file, 2);
        assert_eq!(census.methods_seeded, 2);
        assert_eq!(census.methods_unresolved_class, 0);

        assert_eq!(
            view(&source, 100),
            view(&restored, 100),
            "an identity-policy round trip must reproduce the store exactly",
        );

        // And the bytes are stable: a second save of the SAME store is
        // byte-identical, which is the determinism claim in the module doc.
        let (again, _) = serialize(&source, &namer(100));
        assert_eq!(bytes, again, "serialisation must be deterministic");

        // Round 11 wave 2 (r11-tier-profile-replay-residuals item 3): the
        // restored store's receivers are all replayed seeds no live run has
        // judged, so saving it writes none of them back. They used to be
        // saved at full strength and re-seeded on every load, never ageing out.
        let (resaved, census) = serialize(&restored, &namer(100));
        assert_eq!(
            census.receiver_types_written, 0,
            "unjudged seeds are not evidence"
        );
        assert_ne!(bytes, resaved);
    }

    /// Round 11 wave 18 (r11-tier-profile-replay-residuals item 3, the
    /// invocation half): a save writes the invocations THIS run earned, not
    /// the replay's credit, so the credit of a method the run never called
    /// ages out instead of being re-seeded on every load.
    #[test]
    fn a_save_writes_earned_invocations_not_the_replayed_credit() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let restored = ProfileStore::new();
        if let Err(e) = load_into_with_policy(
            &restored,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        ) {
            assert!(false, "load failed: {e}");
            return;
        }
        let packed = cratonvm_jit_api::invoc_key_parts(102, "handle", "(Ljava/lang/Object;)V");
        assert_eq!(
            restored.add_invocations(packed, 0),
            888,
            "the identity policy credits the whole count"
        );
        let saved_invocations = |bytes: &[u8]| -> Option<u32> {
            decode(bytes)
                .ok()?
                .into_iter()
                .find(|r| r.method_name == "handle")
                .map(|r| r.invocations)
        };
        let (resaved, _) = serialize(&restored, &namer(100));
        // Since round 11 wave 19 the record's replayed branches, call sites and
        // loops are subtracted too, so a method this run never ran has no
        // evidence left and no record at all.
        assert_eq!(
            saved_invocations(&resaved),
            None,
            "no call this run: nothing is written back"
        );
        // Five calls this run land on the same counter as the credit.
        restored.add_invocations(packed, 5);
        let (after_calls, _) = serialize(&restored, &namer(100));
        assert_eq!(saved_invocations(&after_calls), Some(5), "only what was earned");
        // The tests' view keeps the credit, as it keeps unjudged receivers.
        let viewed = view(&restored, 100)
            .into_iter()
            .find(|r| r.method_name == "handle")
            .map(|r| r.invocations);
        assert_eq!(viewed, Some(893));
    }

    /// Round 11 wave 19 (r11-tier-profile-replay-residuals item 3, the branch /
    /// call-site / loop half): a save writes this run's observations at those
    /// sites, not the replayed seed under them, so a replayed hint ages out
    /// instead of being written back and re-seeded on every load. The seed
    /// still serves this run (the in-memory profile keeps it).
    #[test]
    fn a_save_writes_observed_branches_call_sites_and_loops_not_the_seeds() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let restored = ProfileStore::new();
        if let Err(e) = load_into_with_policy(
            &restored,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        ) {
            assert!(false, "load failed: {e}");
            return;
        }
        let handle = key(102, "handle", "(Ljava/lang/Object;)V");
        // The seeds serve this run: `populated` recorded 7 as 2 taken / 1 not.
        let seeded = restored
            .get_profile(&handle)
            .and_then(|p| p.branches.get(&7).cloned())
            .map(|b| (b.taken, b.not_taken));
        assert_eq!(seeded, Some((2, 1)), "the seed is in the live profile");

        // This run executes each kind of site once.
        restored.with_method_profile_mut(&handle, |p| {
            p.record_branch(7, false);
            p.record_call_site(23);
            p.record_backedge(31);
        });
        let (resaved, _) = serialize(&restored, &namer(100));
        let records = decode(&resaved).unwrap_or_default();
        assert_eq!(
            records.iter().map(|r| r.method_name.as_str()).collect::<Vec<_>>(),
            vec!["handle"],
            "`run` observed nothing this run and is not written"
        );
        let handle_record = &records[0];
        assert_eq!(handle_record.branches, vec![(7, 0, 1)], "only the live not-taken");
        assert_eq!(handle_record.call_sites, vec![(23, 1)]);
        assert_eq!(handle_record.loops, vec![(31, 1, 0, 0)]);

        // The tests' view still sees seed plus observation.
        let viewed = view(&restored, 100)
            .into_iter()
            .find(|r| r.method_name == "handle")
            .map(|r| (r.branches, r.call_sites));
        assert_eq!(viewed, Some((vec![(7, 2, 2), (19, 0, 1)], vec![(23, 3)])));
    }

    /// Round 11 wave 2 (r11-tier-profile-replay-residuals item 1): a repeated
    /// key (one class name defined by two loaders) is written once, and a file
    /// that repeats a key anyway seeds it once.
    #[test]
    fn a_repeated_method_key_is_written_and_seeded_once() {
        let store = ProfileStore::new();
        // Two loaders' copies of one class: ids 102 and 202 both name
        // `CLASSES[2]` under the namer below.
        let twin_namer = |id: u32| CLASSES.get((id % 100) as usize).map(|s| s.to_string());
        for (id, calls) in [(102u32, 5u32), (202, 9)] {
            store.with_method_profile_mut(&key(id, "handle", "()V"), |p| {
                p.record_branch(3, true);
            });
            store.add_invocations(
                cratonvm_jit_api::invoc_key_parts(id, "handle", "()V"),
                calls,
            );
        }
        let mut census = SaveCensus::default();
        let records = collect(&store, &twin_namer, &mut census);
        assert_eq!(records.len(), 1, "one record per name");
        assert_eq!(
            records[0].invocations, 9,
            "the hottest copy is the one kept"
        );

        // A hand-made file carrying the record twice seeds it once.
        let doubled = encode(&[records[0].clone(), records[0].clone()]);
        let decoded = match decode(&doubled) {
            Ok(d) => d,
            Err(e) => {
                assert!(false, "a repeated key is dropped, not refused: {e}");
                return;
            }
        };
        assert_eq!(decoded.len(), 1);

        // A repeated pc inside a record is refused.
        let mut bad = records[0].clone();
        bad.branches.push(bad.branches[0]);
        assert!(decode(&encode(&[bad])).is_err());
    }

    /// Round 12 wave 1: two loaders' classes of one NAME at one receiver site
    /// are written as one entry (their total), and a file that repeats a name
    /// at a site anyway seeds that class once, within `receiver_ceiling`.
    #[test]
    fn a_receiver_name_repeated_at_a_site_is_written_and_seeded_once() {
        let store = ProfileStore::new();
        // Ids 103 and 203 both name `CLASSES[3]` (`org/example/Impl`).
        let twin_namer = |id: u32| CLASSES.get((id % 100) as usize).map(|s| s.to_string());
        store.with_method_profile_mut(&key(102, "handle", "()V"), |p| {
            for _ in 0..5 {
                p.record_receiver(11, 103);
            }
            for _ in 0..9 {
                p.record_receiver(11, 203);
            }
        });
        let mut census = SaveCensus::default();
        let records = collect(&store, &twin_namer, &mut census);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].receivers,
            vec![(11u32, vec![(CLASSES[3].to_string(), 14u32)])],
            "one entry per name, carrying the name's total"
        );

        // A hand-made file that names the class three times at the site.
        let mut repeated = records[0].clone();
        repeated.receivers = vec![(
            11,
            vec![
                (CLASSES[3].to_string(), 30),
                (CLASSES[3].to_string(), 30),
                (CLASSES[3].to_string(), 30),
            ],
        )];
        let policy = ReplaySeedPolicy {
            receiver_ceiling: 20,
            ..ReplaySeedPolicy::identity()
        };
        let restored = ProfileStore::new();
        let loaded = load_into_with_policy(
            &restored,
            &encode(&[repeated]),
            &resolver(100),
            &live_len,
            &policy,
        );
        assert!(loaded.is_ok(), "a repeated name is merged, not refused");
        let seeded = restored
            .get_profile(&key(102, "handle", "()V"))
            .and_then(|p| p.receivers.get(&11).and_then(|s| s.get(&103).copied()));
        assert_eq!(seeded, Some(20), "seeded once, at the ceiling, not three times");
    }

    /// Identity is by NAME. The same profile, written under one set of class
    /// ids and read under a completely different set, still lands on the right
    /// method — which is the single most important property of the format, and
    /// the one a `class_id`-keyed format would get wrong every single run.
    #[test]
    fn identity_survives_a_different_class_id_space() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));

        // A second run that numbered its classes from 9000 instead of 100.
        let restored = ProfileStore::new();
        let loaded = load_into_with_policy(
            &restored,
            &bytes,
            &resolver(9000),
            &live_len,
            &ReplaySeedPolicy::identity(),
        );
        assert!(
            loaded.is_ok(),
            "load under a different id space must succeed"
        );

        assert_eq!(
            view(&source, 100),
            view(&restored, 9000),
            "the same method, under different ids, must carry the same profile",
        );

        // Concretely: the receiver seeded at bci 11 must be THIS run's
        // `org/example/Impl` (9003), not the id the file's producer used (103).
        let service = key(9002, "handle", "(Ljava/lang/Object;)V");
        let profile = match restored.get_profile(&service) {
            Some(p) => p,
            None => {
                assert!(false, "the method was not seeded under the new id space");
                return;
            }
        };
        let site = match profile.receivers.get(&11) {
            Some(s) => s,
            None => {
                assert!(false, "bci 11 carries no receiver profile");
                return;
            }
        };
        assert_eq!(site.get(&9003).copied(), Some(40));
        assert_eq!(
            site.get(&103),
            None,
            "the producing run's raw class id must never appear in this run's store",
        );
    }

    /// An empty store is a valid file, and reading it back is "no hints" rather
    /// than an error.
    #[test]
    fn an_empty_profile_is_no_hints_not_an_error() {
        let empty = ProfileStore::new();
        let (bytes, save) = serialize(&empty, &namer(100));
        assert_eq!(save.methods_written, 0);

        let target = ProfileStore::new();
        match load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        ) {
            Ok(census) => {
                assert_eq!(census.methods_in_file, 0);
                assert_eq!(census.methods_seeded, 0);
                assert!(!census.file_missing);
            }
            Err(e) => assert!(false, "an empty profile must load: {e}"),
        }
        assert_eq!(replay_outcome(&target), ReplayOutcome::default());
    }

    /// A missing file is "no hints", not an error, at the call site.
    #[test]
    fn a_missing_file_is_no_hints_not_an_error() {
        let store = ProfileStore::new();
        let mut path = std::env::temp_dir();
        path.push("cratonvm-profile-store-does-not-exist-20260916.bin");
        let _ = std::fs::remove_file(&path);
        match load_from_path(&store, &path, &resolver(100), &live_len) {
            Ok(census) => {
                assert!(census.file_missing);
                assert_eq!(census.methods_seeded, 0);
            }
            Err(e) => assert!(false, "a missing file must not be an error: {e}"),
        }
    }

    /// r11-tier: a path that is not a regular file is refused up front instead
    /// of being read to an end-of-file it may never have (`/dev/zero`, a FIFO).
    /// A directory stands in for them portably.
    #[test]
    fn a_path_that_is_not_a_regular_file_is_refused() {
        let store = ProfileStore::new();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        match load_from_path(&store, dir, &resolver(100), &live_len) {
            Err(ProfileStoreError::Format(m)) => assert!(m.contains("not a regular file"), "{m}"),
            Err(e) => assert!(false, "a directory must be refused as one: {e}"),
            Ok(census) => assert!(false, "a directory must not load: {census:?}"),
        }
    }

    /// A method whose class is not defined at load time waits for the class's
    /// definition and is seeded then -- the load happens before any
    /// application class exists, so dropping it dropped the application
    /// profile.
    #[test]
    fn a_method_whose_class_is_defined_later_is_seeded_at_definition() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        // Load time: only bootstrap `java/lang/Object` and `HashMap` exist.
        let at_load = |name: &str| -> Option<u32> {
            match name {
                "java/lang/Object" => Some(200),
                "java/util/HashMap" => Some(201),
                _ => None,
            }
        };
        let census = load_into_with_policy(
            &target,
            &bytes,
            &at_load,
            &live_len,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!(census.methods_seeded, 0);
        assert_eq!(census.methods_unresolved_class, 2);
        assert_eq!(replay_pending_census(&target), (2, 2, 0, 0, 0));
        let service = key(202, "handle", "(Ljava/lang/Object;)V");
        assert!(
            target
                .get_profile(&service)
                .map_or(true, |p| p.branches.is_empty()),
            "nothing seeded before the definition"
        );

        // A definition of an unrelated class seeds nothing.
        assert_eq!(
            seed_pending_replay_for_class(
                &target,
                "org/example/Unrelated",
                &resolver(200),
                &live_len
            ),
            0
        );
        // `Service` is defined; its receiver `Impl` is not yet.
        let after_service = |name: &str| -> Option<u32> {
            match name {
                "org/example/Impl" => None,
                other => resolver(200)(other),
            }
        };
        assert_eq!(
            seed_pending_replay_for_class(
                &target,
                "org/example/Service",
                &after_service,
                &live_len
            ),
            1
        );
        let (classes, methods, seeded, receivers_dropped, _) = replay_pending_census(&target);
        assert_eq!((classes, methods, seeded), (1, 1, 1));
        assert!(
            receivers_dropped > 0,
            "the receiver named an undefined class"
        );
        let seeded_profile = target.get_profile(&service).expect("seeded at definition");
        assert!(!seeded_profile.branches.is_empty());
        // A repeat definition hook finds nothing left.
        assert_eq!(
            seed_pending_replay_for_class(
                &target,
                "org/example/Service",
                &resolver(200),
                &live_len
            ),
            0
        );

        // The sweep form picks up the rest once `Impl` resolves.
        assert_eq!(seed_pending_replay(&target, &resolver(200), &live_len), 1);
        assert_eq!(
            replay_pending_census(&target),
            (0, 0, 2, receivers_dropped, 0)
        );
        assert!(target.get_profile(&key(203, "run", "()I")).is_some());
    }

    /// An ambiguous (or still unresolvable) name leaves its records waiting.
    #[test]
    fn an_unresolvable_definition_leaves_the_records_waiting() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let nothing = |_: &str| -> Option<u32> { None };
        load_into_with_policy(
            &target,
            &bytes,
            &nothing,
            &live_len,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!(
            seed_pending_replay_for_class(&target, "org/example/Service", &nothing, &live_len),
            0
        );
        assert_eq!(replay_pending_census(&target), (2, 2, 0, 0, 0));
    }

    /// A method whose class this run never loaded is not seeded, is counted,
    /// and costs the rest of the file nothing.
    #[test]
    fn a_method_naming_an_unloaded_class_is_dropped_and_counted() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        // A resolver that only knows `org/example/Impl`.
        let partial = |name: &str| -> Option<u32> {
            if name == "org/example/Impl" {
                Some(7)
            } else {
                None
            }
        };
        match load_into_with_policy(
            &target,
            &bytes,
            &partial,
            &live_len,
            &ReplaySeedPolicy::identity(),
        ) {
            Ok(census) => {
                assert_eq!(census.methods_seeded, 1, "only Impl.run should be seeded");
                assert_eq!(census.methods_unresolved_class, 1);
            }
            Err(e) => assert!(false, "a partially resolvable file must still load: {e}"),
        }
    }

    // ── stale records (version 2) ───────────────────────────────────────────

    /// r11-tier-profile-replay-residuals item 4: a record saved against
    /// bytecode of a different length is dropped and counted, at both seeding
    /// doors, and the matching record beside it is still seeded.
    #[test]
    fn a_record_saved_against_other_bytecode_is_dropped_as_stale() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(
            decoded.iter().map(|r| r.code_len).collect::<Vec<_>>(),
            vec![8, 64],
            "the stamped lengths are written (Impl.run, then Service.handle)"
        );

        // `handle` grew by one byte since the save.
        let grown = |_: u32, name: &str, _: &str| -> LiveCode {
            match name {
                "handle" => LiveCode::Len(65),
                _ => LiveCode::Len(8),
            }
        };
        let eager = ProfileStore::new();
        let census = load_into_with_policy(
            &eager,
            &bytes,
            &resolver(100),
            &grown,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
        assert!(eager
            .get_profile(&key(102, "handle", "(Ljava/lang/Object;)V"))
            .is_none());
        assert!(eager.get_profile(&key(103, "run", "()I")).is_some());

        // The definition door: `run` no longer exists in the live class.
        let deferred = ProfileStore::new();
        let nothing = |_: &str| -> Option<u32> { None };
        load_into_with_policy(
            &deferred,
            &bytes,
            &nothing,
            &live_len,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        let gone = |_: u32, _: &str, _: &str| LiveCode::Absent;
        assert_eq!(
            seed_pending_replay_for_class(&deferred, "org/example/Impl", &resolver(200), &gone),
            0
        );
        assert_eq!(replay_pending_census(&deferred), (1, 1, 0, 0, 1));
    }

    /// A record whose saving run did not know the length (`code_len` 0) is
    /// still refused when a pc lies past the live code; a busy lookup leaves
    /// the record waiting instead of dropping it.
    #[test]
    fn an_unknown_length_is_bounded_by_its_pcs_and_a_busy_lookup_waits() {
        let source = ProfileStore::new();
        source.with_method_profile_mut(&key(102, "spin", "()V"), |p| {
            p.record_branch(40, true);
        });
        let (bytes, _) = serialize(&source, &namer(100));
        assert_eq!(decode(&bytes).expect("decodes")[0].code_len, 0);

        let short = |_: u32, _: &str, _: &str| LiveCode::Len(16);
        let target = ProfileStore::new();
        let census = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &short,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((census.methods_seeded, census.methods_stale_code), (0, 1));

        let busy = |_: u32, _: &str, _: &str| LiveCode::Busy;
        let waiting = ProfileStore::new();
        let census = load_into_with_policy(
            &waiting,
            &bytes,
            &resolver(100),
            &busy,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!(
            (
                census.methods_seeded,
                census.methods_stale_code,
                census.methods_unresolved_class
            ),
            (0, 0, 1)
        );
        assert_eq!(
            seed_pending_replay_for_class(&waiting, "org/example/Service", &resolver(100), &busy),
            0
        );
        assert_eq!(
            replay_pending_census(&waiting),
            (1, 1, 0, 0, 0),
            "put back, not dropped"
        );
        assert_eq!(
            seed_pending_replay_for_class(
                &waiting,
                "org/example/Service",
                &resolver(100),
                &live_len
            ),
            1
        );
        let seeded = waiting
            .get_profile(&key(102, "spin", "()V"))
            .expect("seeded once the lookup answered");
        assert_eq!(
            seeded.recorded_code_len(),
            256,
            "the seed stamps the live length"
        );
    }

    /// The fast-path branch counters stamp the length; two recorders that
    /// disagree about it mark the profile, and such a record never seeds.
    #[test]
    fn a_profile_mixing_two_code_lengths_is_saved_but_never_seeded() {
        let source = ProfileStore::new();
        source.with_method_profile_mut(&key(102, "hot", "()V"), |p| {
            p.flat_branches_for(20).record(3, true);
        });
        source.with_method_profile_mut(&key(102, "mixed", "()V"), |p| {
            p.flat_branches_for(20).record(3, true);
            let _ = p.flat_branches_for(24);
        });
        let (bytes, _) = serialize(&source, &namer(100));
        let lens: Vec<u32> = decode(&bytes)
            .expect("decodes")
            .iter()
            .map(|r| r.code_len)
            .collect();
        assert_eq!(lens, vec![20, crate::profile::CODE_LEN_CONFLICT]);

        let any = |_: u32, _: &str, _: &str| LiveCode::Len(20);
        let target = ProfileStore::new();
        let census = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &any,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
    }

    /// A version 1 file is refused with the reason, not as a generic unknown
    /// version.
    #[test]
    fn a_version_1_file_is_refused_with_its_reason() {
        let mut bytes = one_of_everything();
        bytes[4] = 1;
        bytes[5] = 0;
        let message = err_of(&bytes);
        assert!(
            message.contains("version 1 carries no per-method code length"),
            "expected the version-1 refusal, got {message:?}",
        );
    }

    // ── exact records (version 3) ───────────────────────────────────────────

    /// The hash is FNV-1a 64 and must stay it: a file written by one build
    /// is read by another, so the digest is part of the format.
    #[test]
    fn the_bytecode_hash_is_fnv1a_64_and_never_zero() {
        assert_eq!(bytecode_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(bytecode_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(bytecode_hash(&[0x2a, 0xb7]), bytecode_hash(&[0x2b, 0xb7]));
    }

    /// Round 11 wave 14 (r11-tier-profile-replay-residuals item 4, the hash
    /// half): a save stamps each record with the saving run's live method, and
    /// a seeding door drops a record whose bytes changed at the same length.
    /// A door that knows only the length still seeds it.
    #[test]
    fn a_same_length_edit_is_dropped_by_the_hash() {
        let source = populated(100);
        let saved_code = |_: u32, name: &str, _: &str| -> LiveCode {
            match name {
                "handle" => LiveCode::Code {
                    len: 64,
                    hash: 0x1111,
                },
                _ => LiveCode::Code {
                    len: 8,
                    hash: 0x2222,
                },
            }
        };
        let (bytes, _) = serialize_against_live_code(&source, &namer(100), &saved_code);
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(
            decoded
                .iter()
                .map(|r| (r.code_len, r.code_hash))
                .collect::<Vec<_>>(),
            vec![(8, 0x2222), (64, 0x1111)],
            "Impl.run, then Service.handle"
        );

        // `handle` was edited in place: same length, other bytes.
        let edited = |_: u32, name: &str, _: &str| -> LiveCode {
            match name {
                "handle" => LiveCode::Code {
                    len: 64,
                    hash: 0x3333,
                },
                _ => LiveCode::Code {
                    len: 8,
                    hash: 0x2222,
                },
            }
        };
        let target = ProfileStore::new();
        let census = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &edited,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
        assert!(target
            .get_profile(&key(102, "handle", "(Ljava/lang/Object;)V"))
            .is_none());

        // A length-only answer cannot see the edit, so both seed.
        let length_only = ProfileStore::new();
        let census = load_into_with_policy(
            &length_only,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((census.methods_seeded, census.methods_stale_code), (2, 0));
    }

    /// A record no length-aware recorder touched (receiver evidence only) is
    /// stamped from the live method at save; a record recorded against
    /// another length than the live one is written as a conflict and never
    /// seeds.
    #[test]
    fn a_save_stamps_unknown_lengths_live_and_refuses_another_version() {
        let source = ProfileStore::new();
        source.with_method_profile_mut(&key(102, "dispatch", "()V"), |p| {
            p.record_receiver(4, 103);
        });
        source.with_method_profile_mut(&key(102, "old", "()V"), |p| {
            p.note_code_len(8);
            p.record_branch(2, true);
        });
        let live = |_: u32, name: &str, _: &str| -> LiveCode {
            match name {
                "dispatch" => LiveCode::Code {
                    len: 12,
                    hash: 0x44,
                },
                _ => LiveCode::Code {
                    len: 16,
                    hash: 0x55,
                },
            }
        };
        let (bytes, _) = serialize_against_live_code(&source, &namer(100), &live);
        let decoded = decode(&bytes).expect("decodes");
        assert_eq!(
            decoded
                .iter()
                .map(|r| (r.method_name.as_str(), r.code_len, r.code_hash))
                .collect::<Vec<_>>(),
            vec![
                ("dispatch", 12, 0x44),
                ("old", crate::profile::CODE_LEN_CONFLICT, 0),
            ]
        );
        // A busy or absent live answer keeps what the recorders stamped.
        assert_eq!(stamp_record_code(8, LiveCode::Busy), (8, 0));
        assert_eq!(stamp_record_code(0, LiveCode::Absent), (0, 0));
        assert_eq!(stamp_record_code(8, LiveCode::Len(8)), (8, 0));
    }

    /// A version 2 file (length, no hash) is still read: its records keep the
    /// length check and seed against a hashing door.
    #[test]
    fn a_version_2_file_is_read_with_every_hash_unknown() {
        let source = populated(100);
        let mut census = SaveCensus::default();
        let records = collect(&source, &namer(100), &mut census);
        let v2 = encode_as(&records, 2);
        let decoded = decode(&v2).expect("a version 2 file decodes");
        assert!(decoded.iter().all(|r| r.code_hash == 0));
        assert_eq!(
            decoded.iter().map(|r| r.code_len).collect::<Vec<_>>(),
            vec![8, 64]
        );
        let hashing = |_: u32, name: &str, _: &str| -> LiveCode {
            match name {
                "handle" => LiveCode::Code {
                    len: 64,
                    hash: 0x77,
                },
                _ => LiveCode::Code { len: 8, hash: 0x88 },
            }
        };
        let target = ProfileStore::new();
        let loaded = load_into_with_policy(
            &target,
            &v2,
            &resolver(100),
            &hashing,
            &ReplaySeedPolicy::identity(),
        )
        .expect("loads");
        assert_eq!((loaded.methods_seeded, loaded.methods_stale_code), (2, 0));
        // Every truncation of a version 2 file is refused too.
        for cut in 0..v2.len() {
            assert!(
                decode(&v2[..cut]).is_err(),
                "v2 truncated to {cut} accepted"
            );
        }
    }

    // ── hostile input ───────────────────────────────────────────────────────

    fn header(method_count: u32) -> Vec<u8> {
        let mut b = Vec::new();
        put_u32(&mut b, MAGIC);
        put_u16(&mut b, VERSION);
        put_u16(&mut b, RESERVED);
        put_u32(&mut b, method_count);
        b
    }

    fn err_of(bytes: &[u8]) -> String {
        match decode(bytes) {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_bad_magic_is_refused() {
        let mut bytes = one_of_everything();
        bytes[0] ^= 0xFF;
        let message = err_of(&bytes);
        assert!(
            message.contains("bad magic"),
            "expected a magic refusal, got {message:?}",
        );
    }

    #[test]
    fn a_bad_version_is_refused() {
        let mut bytes = one_of_everything();
        bytes[4] = 0xEE;
        bytes[5] = 0xFF;
        let message = err_of(&bytes);
        assert!(
            message.contains("unsupported profile version"),
            "expected a version refusal, got {message:?}",
        );
    }

    #[test]
    fn a_non_zero_reserved_field_is_refused() {
        let mut bytes = one_of_everything();
        bytes[6] = 1;
        let message = err_of(&bytes);
        assert!(
            message.contains("reserved header field"),
            "expected a reserved-field refusal, got {message:?}",
        );
    }

    /// `u32::MAX` methods is refused at the four bytes that declare it, by the
    /// `MAX_METHODS` cap, before any collection is sized from it.
    #[test]
    fn a_method_count_of_u32_max_is_refused_at_the_cap() {
        let blob = header(u32::MAX);
        let message = err_of(&blob);
        assert!(
            message.contains("exceeds this format's cap"),
            "expected the cap to refuse it, got {message:?}",
        );
    }

    /// `u32::MAX` receiver types is refused the same way. Before the cap
    /// existed this would have asked the allocator for
    /// 4 294 967 295 × `size_of::<(String, u32)>()` — about 137 GiB — from a
    /// few dozen bytes of input.
    #[test]
    fn a_receiver_type_count_of_u32_max_is_refused_at_the_cap() {
        let mut body = Vec::new();
        put_str(&mut body, "org/example/Service");
        put_str(&mut body, "handle");
        put_str(&mut body, "()V");
        put_u32(&mut body, 0); // code_len
        put_u64(&mut body, 0); // code_hash (version 3)
        put_u32(&mut body, 0); // invocations
        put_u32(&mut body, 0); // branches
        put_u32(&mut body, 0); // call sites
        put_u32(&mut body, 0); // loops
        put_u32(&mut body, 1); // one receiver site
        put_u32(&mut body, 11); // pc
        put_u32(&mut body, u32::MAX); // ← the hostile count

        let mut blob = header(1);
        put_u32(&mut blob, body.len() as u32);
        blob.extend_from_slice(&body);

        let message = err_of(&blob);
        assert!(
            message.contains("receiver type count") && message.contains("cap"),
            "expected the receiver-type cap to refuse it, got {message:?}",
        );
    }

    /// A count that passes its `MAX_*` cap but is larger than the bytes behind
    /// it is refused by the bytes-remaining bound. This is the bound that
    /// actually holds, and the only one that makes the surviving
    /// `Vec::with_capacity` safe rather than merely unlikely to hurt.
    #[test]
    fn a_count_inside_the_cap_but_past_the_bytes_is_refused() {
        // 4 096 receiver types is exactly MAX_RECEIVER_TYPES_PER_SITE, so the
        // cap passes it and only the second bound can refuse it.
        let mut body = Vec::new();
        put_str(&mut body, "org/example/Service");
        put_str(&mut body, "handle");
        put_str(&mut body, "()V");
        put_u32(&mut body, 0); // code_len
        put_u64(&mut body, 0); // code_hash (version 3)
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 1);
        put_u32(&mut body, 11);
        put_u32(&mut body, MAX_RECEIVER_TYPES_PER_SITE as u32);
        // ...and then nothing. 4 096 types need at least 32 768 bytes.

        let mut blob = header(1);
        put_u32(&mut blob, body.len() as u32);
        blob.extend_from_slice(&body);

        let message = err_of(&blob);
        assert!(
            message.contains("cannot fit in the"),
            "expected the bytes-remaining bound to refuse it, got {message:?}",
        );
    }

    /// The same, one level up: a method count inside `MAX_METHODS` but larger
    /// than `remaining / MIN_METHOD_BYTES`.
    #[test]
    fn a_method_count_inside_the_cap_but_past_the_bytes_is_refused() {
        let blob = header(1000);
        let message = err_of(&blob);
        assert!(
            message.contains("cannot fit in the"),
            "expected the bytes-remaining bound to refuse it, got {message:?}",
        );
    }

    /// A declared string length beyond the `CONSTANT_Utf8` ceiling is refused
    /// at the length, before the cursor is advanced by it. On a 32-bit target
    /// this is also the case that would wrap `pos + len` without `checked_add`.
    #[test]
    fn a_u32_max_string_length_is_refused_at_the_length() {
        let mut body = Vec::new();
        put_u32(&mut body, u32::MAX); // class name length
                                      // Pad so the record is at least `MIN_METHOD_BYTES` long. Without the
                                      // padding the METHOD count's bytes-remaining bound refuses the file
                                      // first, and this test would pass while checking a different bound.
        body.resize(MIN_METHOD_BYTES - 4, 0);
        let mut blob = header(1);
        put_u32(&mut blob, body.len() as u32);
        blob.extend_from_slice(&body);
        let message = err_of(&blob);
        assert!(
            message.contains("exceeds the") && message.contains("cap"),
            "expected the string cap to refuse it, got {message:?}",
        );
    }

    /// Invalid UTF-8 is an `Err`, never a panic.
    #[test]
    fn invalid_utf8_is_an_error() {
        let mut body = Vec::new();
        put_u32(&mut body, 4);
        body.extend_from_slice(&[0xF0, 0x28, 0x8C, 0x28]); // not UTF-8
        put_str(&mut body, "handle");
        put_str(&mut body, "()V");
        put_u32(&mut body, 0); // code_len
        put_u64(&mut body, 0); // code_hash (version 3)
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);

        let mut blob = header(1);
        put_u32(&mut blob, body.len() as u32);
        blob.extend_from_slice(&body);

        let message = err_of(&blob);
        assert!(
            message.contains("not UTF-8"),
            "expected a UTF-8 refusal, got {message:?}",
        );
    }

    /// **Every** prefix of a well-formed file is an error. Not a sample of
    /// prefixes: every one, because a truncation bound that holds at some
    /// offsets is a truncation bound that does not hold.
    #[test]
    fn every_truncation_is_an_error() {
        let bytes = one_of_everything();
        assert!(bytes.len() > 64, "the fixture must exercise several arrays");
        for cut in 0..bytes.len() {
            assert!(
                decode(&bytes[..cut]).is_err(),
                "a file truncated to {cut} of {} byte(s) was accepted",
                bytes.len(),
            );
        }
        assert!(
            decode(&bytes).is_ok(),
            "the untruncated fixture must still parse",
        );
    }

    /// Trailing bytes after the last record are refused. Version 1 is strict
    /// about this; the module doc says why, and says which future change
    /// relaxes it.
    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = one_of_everything();
        bytes.push(0);
        let message = err_of(&bytes);
        assert!(
            message.contains("trailing byte"),
            "expected a trailing-byte refusal, got {message:?}",
        );
    }

    /// A record that declares more bytes than its fields consume is refused,
    /// which is what makes the length delimiter a check rather than decoration.
    #[test]
    fn a_record_longer_than_its_fields_is_refused() {
        let mut body = Vec::new();
        put_str(&mut body, "org/example/Service");
        put_str(&mut body, "handle");
        put_str(&mut body, "()V");
        put_u32(&mut body, 0); // code_len
        put_u64(&mut body, 0); // code_hash (version 3)
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        put_u32(&mut body, 0);
        body.push(0xAA); // one byte no field claims

        let mut blob = header(1);
        put_u32(&mut blob, body.len() as u32);
        blob.extend_from_slice(&body);

        let message = err_of(&blob);
        assert!(
            message.contains("must be exactly its declared length"),
            "expected a record-length refusal, got {message:?}",
        );
    }

    /// A corrupt count inside one record cannot reach past that record: the
    /// error names the record, and the bound that refused it is the record's
    /// own length rather than the file's.
    #[test]
    fn a_corrupt_count_is_contained_within_its_record() {
        let mut bytes = one_of_everything();
        // The header is 12 bytes (magic, version, reserved, method count), so
        // the first record's length prefix starts there. The last four bytes of
        // that record's body are its final array count — the receiver-site
        // count — and clobbering them with `u32::MAX` is a corruption strictly
        // INSIDE record 0. The point of the assertion is the prefix on the
        // message: the failure is attributed to the record, which is what the
        // length delimiter buys.
        let record_len_at = 12;
        let len_bytes = [
            bytes[record_len_at],
            bytes[record_len_at + 1],
            bytes[record_len_at + 2],
            bytes[record_len_at + 3],
        ];
        let record_len = u32::from_le_bytes(len_bytes) as usize;
        let last_count_at = record_len_at + 4 + record_len - 4;
        bytes[last_count_at..last_count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let message = err_of(&bytes);
        assert!(
            message.contains("method record 0"),
            "the error must name the record it came from, got {message:?}",
        );
    }

    // ── replay is a hint ────────────────────────────────────────────────────

    /// A replayed receiver is marked `from_replay`; a live observation of the
    /// SAME class confirms it and the census moves.
    #[test]
    fn a_confirmed_receiver_keeps_its_seed_and_moves_the_census() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let _ = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        );

        let service = key(102, "handle", "(Ljava/lang/Object;)V");
        let before = replay_outcome(&target);
        assert!(before.methods_seeded > 0);
        assert_eq!(before.receiver_types_confirmed, 0);
        assert_eq!(before.receiver_types_refuted, 0);
        assert_eq!(
            before.receiver_types_unjudged, before.receiver_types_replayed,
            "nothing has executed yet, so nothing is judged",
        );

        // The live run sees `org/example/Impl` (103) at bci 11 — the class the
        // replay predicted.
        target.with_method_profile_mut(&service, |p| {
            assert!(
                p.is_from_replay(),
                "the seeded method must carry provenance"
            );
            p.record_receiver(11, 103);
        });

        let after = replay_outcome(&target);
        assert_eq!(after.receiver_types_confirmed, 1);
        assert_eq!(after.receiver_types_refuted, 0);
        assert_eq!(after.methods_shape_contradicted, 0);

        // The seed survives confirmation — that is the point of confirming it.
        match target
            .get_profile(&service)
            .and_then(|p| p.receivers.get(&11).map(|s| s.get(&103).copied()))
        {
            Some(Some(n)) => assert_eq!(n, 41, "40 seeded plus the one live observation"),
            _ => assert!(false, "the confirmed seed must still be there"),
        }
    }

    /// r11-tier: a SECOND observation of an already-confirmed class is not a
    /// contradiction. Confirming `Impl` took it off the pending list, so before
    /// the fix the next `Impl` read as a class the replay never named and
    /// refuted `Other`'s tail seed — every replayed bimorphic site lost its
    /// tail on its first repeat.
    #[test]
    fn a_confirmed_class_seen_again_refutes_nothing() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let _ = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        );

        let service = key(102, "handle", "(Ljava/lang/Object;)V");
        target.with_method_profile_mut(&service, |p| {
            p.record_receiver(11, 103);
            p.record_receiver(11, 103);
            p.record_receiver(11, 103);
        });

        let after = replay_outcome(&target);
        assert_eq!(after.receiver_types_confirmed, 1);
        assert_eq!(after.receiver_types_refuted, 0, "Impl was named by the replay");
        assert_eq!(after.methods_shape_contradicted, 0);
        assert_eq!(
            target
                .get_profile(&service)
                .and_then(|p| p.receivers.get(&11).and_then(|s| s.get(&104).copied())),
            Some(3),
            "Other's tail seed must survive a repeat of the confirmed class",
        );

        // A class the replay never named still refutes what is pending.
        target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));
        let refuted = replay_outcome(&target);
        assert_eq!(refuted.receiver_types_refuted, 1, "only Other was still pending");
        assert_eq!(refuted.methods_shape_contradicted, 1);
    }

    /// A live contradiction CORRECTS the replayed shape: the still-pending
    /// seeds at that site are subtracted back out, the method is marked
    /// contradicted, and the census moves from unjudged to refuted.
    #[test]
    fn a_live_contradiction_corrects_the_replayed_shape() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let _ = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        );

        let service = key(102, "handle", "(Ljava/lang/Object;)V");
        // The live run sees `java/util/HashMap` (101) at bci 11 — a class the
        // replay never named there.
        target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));

        let after = replay_outcome(&target);
        assert_eq!(
            after.receiver_types_refuted, 2,
            "both seeds at bci 11 (Impl and Other) are refuted",
        );
        assert_eq!(after.methods_shape_contradicted, 1);
        assert_eq!(after.receiver_types_confirmed, 0);

        let profile = match target.get_profile(&service) {
            Some(p) => p,
            None => {
                assert!(false, "the method must still exist");
                return;
            }
        };
        let site = match profile.receivers.get(&11) {
            Some(s) => s,
            None => {
                assert!(false, "the live observation must keep the site alive");
                return;
            }
        };
        assert_eq!(
            site.get(&103),
            None,
            "the refuted seed must be subtracted back out, not averaged in",
        );
        assert_eq!(site.get(&104), None, "and so must the tail seed");
        assert_eq!(
            site.get(&101).copied(),
            Some(1),
            "the live observation that did the refuting must survive",
        );

        // A second contradicting observation must not re-count: both seeds have
        // already been judged.
        target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));
        assert_eq!(replay_outcome(&target).receiver_types_refuted, 2);
    }

    /// A site the replay did not name at all is untouched by replay accounting:
    /// a live observation there is neither a confirmation nor a refutation.
    #[test]
    fn an_unseeded_site_is_not_replay_accounted() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let _ = load_into_with_policy(
            &target,
            &bytes,
            &resolver(100),
            &live_len,
            &ReplaySeedPolicy::identity(),
        );
        let service = key(102, "handle", "(Ljava/lang/Object;)V");
        target.with_method_profile_mut(&service, |p| p.record_receiver(999, 101));
        let after = replay_outcome(&target);
        assert_eq!(after.receiver_types_confirmed, 0);
        assert_eq!(after.receiver_types_refuted, 0);
        assert_eq!(after.methods_shape_contradicted, 0);
    }

    // ── the seeding policy ──────────────────────────────────────────────────

    /// A replayed invocation count can never, on its own, reach the gate it
    /// feeds. This is the claim `ReplaySeedPolicy::conservative` makes, and it
    /// has to hold at every gate, including the small ones a bisecting
    /// developer sets.
    #[test]
    fn a_replayed_invocation_count_cannot_reach_the_gate_alone() {
        for gate in [1u32, 2, 4, 5, 9, 50, 500, 20_000, u32::MAX] {
            let policy = ReplaySeedPolicy::conservative(gate);
            assert!(
                policy.invocation_ceiling < gate.max(1) || gate <= 1,
                "ceiling {} must be below gate {gate}",
                policy.invocation_ceiling,
            );
            for recorded in [0u32, 1, 499, 500, 1_000_000, u32::MAX] {
                let seeded = policy.seed_invocations(recorded);
                assert!(
                    seeded <= policy.invocation_ceiling,
                    "seed {seeded} exceeded the ceiling at gate {gate}",
                );
                if gate > 1 {
                    assert!(
                        seeded < gate,
                        "a seed of {seeded} would reach the gate of {gate} on its own",
                    );
                }
            }
        }
    }

    /// At the default gate the seed is a quarter of the recorded count, capped
    /// at a fifth of the gate — so a method that made 888 calls last run starts
    /// this run needing 400 real ones instead of 500.
    #[test]
    fn the_default_policy_seeds_a_bounded_fraction() {
        let policy = ReplaySeedPolicy::conservative(500);
        assert_eq!(policy.invocation_ceiling, 100);
        assert_eq!(policy.seed_invocations(888), 100);
        assert_eq!(policy.seed_invocations(200), 50);
        assert_eq!(policy.seed_invocations(3), 0);

        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        match load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy) {
            Ok(census) => assert_eq!(census.invocations_seeded, 100),
            Err(e) => assert!(false, "load failed: {e}"),
        }
        let packed = cratonvm_jit_api::invoc_key_parts(102, "handle", "(Ljava/lang/Object;)V");
        // `add_invocations(key, 0)` is the read: it returns the counter's value
        // after adding nothing, which is the seeded credit. The store has no
        // plain getter and does not need one for the live path.
        assert_eq!(target.add_invocations(packed, 0), 100);
        assert_eq!(replay_outcome(&target).invocations_seeded, 100);
    }

    /// A replayed branch hint is exactly as revisable as a live one: the
    /// ceiling is low enough that live observations outvote it.
    #[test]
    fn a_replayed_branch_hint_is_revisable() {
        let source = ProfileStore::new();
        let hot = key(102, "loop", "()V");
        source.with_method_profile_mut(&hot, |p| {
            // Last run: overwhelmingly taken, a million observations.
            p.seed_replayed_branch(4, 1_000_000, 0);
            // A SOURCE store: the seed stands in for last run's live evidence,
            // which a save writes (a save subtracts replayed seeds).
            p.replay = None;
        });
        let (bytes, _) = serialize(&source, &namer(100));

        let target = ProfileStore::new();
        let policy = ReplaySeedPolicy::conservative(500);
        let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

        let seeded = match target
            .get_profile(&hot)
            .and_then(|p| p.branches.get(&4).cloned())
        {
            Some(b) => b,
            None => {
                assert!(false, "the branch must be seeded");
                return;
            }
        };
        assert_eq!(seeded.taken, policy.branch_ceiling);
        assert!(seeded.is_usually_taken());

        // This run's traffic goes the other way. Twice the ceiling of live
        // not-taken observations flips the hint.
        target.with_method_profile_mut(&hot, |p| {
            for _ in 0..(policy.branch_ceiling * 2) {
                p.record_branch(4, false);
            }
        });
        let now = match target
            .get_profile(&hot)
            .and_then(|p| p.branches.get(&4).cloned())
        {
            Some(b) => b,
            None => {
                assert!(false, "the branch must still be there");
                return;
            }
        };
        assert!(
            !now.is_usually_taken(),
            "live observation must be able to outvote a replayed branch hint",
        );
    }

    /// r11-tier: a capped seed keeps its RATIO. Capping each direction (and
    /// each receiver class) on its own seeded a 5 000 : 1 branch as 50/50 and a
    /// 200 : 1 monomorphic site as an even bimorphic one.
    #[test]
    fn capping_a_seed_preserves_its_ratio() {
        assert_eq!(scale_to_ceiling(1_000_000, 1_000_000, 100), 100);
        assert_eq!(scale_to_ceiling(200, 1_000_000, 100), 1, "non-zero stays non-zero");
        assert_eq!(scale_to_ceiling(0, 1_000_000, 100), 0);
        assert_eq!(scale_to_ceiling(40, 60, 100), 40, "under the ceiling: unchanged");
        assert_eq!(scale_to_ceiling(u32::MAX, u32::MAX, 1_000), 1_000);

        let source = ProfileStore::new();
        let hot = key(102, "branchy", "()V");
        source.with_method_profile_mut(&hot, |p| {
            p.seed_replayed_branch(4, 1_000_000, 200_000);
            // A SOURCE store (see `a_replayed_branch_hint_is_revisable`).
            p.replay = None;
            for _ in 0..5_000 {
                p.record_receiver(9, 103);
            }
            for _ in 0..25 {
                p.record_receiver(9, 104);
            }
        });
        let (bytes, _) = serialize(&source, &namer(100));
        let target = ProfileStore::new();
        let policy = ReplaySeedPolicy::conservative(500);
        let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

        let profile = match target.get_profile(&hot) {
            Some(p) => p,
            None => {
                assert!(false, "the method must be seeded");
                return;
            }
        };
        match profile.branches.get(&4) {
            Some(b) => {
                assert_eq!(b.taken, policy.branch_ceiling);
                assert_eq!(b.not_taken, policy.branch_ceiling / 5, "5 : 1 survives the cap");
            }
            None => assert!(false, "the branch must be seeded"),
        }
        match profile.receivers.get(&9) {
            Some(site) => {
                assert_eq!(site.get(&103).copied(), Some(policy.receiver_ceiling));
                assert_eq!(
                    site.get(&104).copied(),
                    Some(policy.receiver_ceiling / 200),
                    "200 : 1 survives the cap",
                );
            }
            None => assert!(false, "the receiver site must be seeded"),
        }
    }

    /// The loop seed preserves the AVERAGE trip count when it caps the entry
    /// count, because the average is the only thing the unroll heuristic reads.
    #[test]
    fn capping_a_loop_seed_preserves_its_average() {
        let source = ProfileStore::new();
        let looper = key(102, "spin", "()V");
        source.with_method_profile_mut(&looper, |p| {
            // 50 000 entries averaging 8 trips each.
            p.seed_replayed_loop(3, 400_000, 50_000, 400_000);
            // A SOURCE store (see `a_replayed_branch_hint_is_revisable`).
            p.replay = None;
        });
        let (bytes, _) = serialize(&source, &namer(100));

        let target = ProfileStore::new();
        let policy = ReplaySeedPolicy::conservative(500);
        let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

        let lp = match target
            .get_profile(&looper)
            .and_then(|p| p.loops.get(&3).cloned())
        {
            Some(l) => l,
            None => {
                assert!(false, "the loop must be seeded");
                return;
            }
        };
        assert_eq!(lp.entry_count, policy.loop_entry_ceiling);
        assert!(
            (lp.avg_trip_count() - 8.0).abs() < 0.01,
            "capping must preserve the average, got {}",
            lp.avg_trip_count(),
        );
    }

    /// A receiver seed is capped, so a stale monomorphic reading is outvotable
    /// — but the cap is above `INLINE_MIN_SPECULATION_OBSERVATIONS` (250), so
    /// replay is not made useless by its own conservatism.
    #[test]
    fn the_receiver_ceiling_is_usable_but_outvotable() {
        let policy = ReplaySeedPolicy::conservative(500);
        assert!(
            policy.receiver_ceiling > 250,
            "a ceiling at or below the speculation floor makes receiver replay a no-op",
        );
        assert!(
            policy.receiver_ceiling <= 10_000,
            "a ceiling this high stops live traffic being able to outvote the seed",
        );
    }

    // ── file round trip ─────────────────────────────────────────────────────

    /// The whole path, through a real file.
    #[test]
    fn save_and_load_through_a_file() {
        let source = populated(100);
        let mut path = std::env::temp_dir();
        path.push("cratonvm-profile-store-roundtrip-20260916.bin");
        let _ = std::fs::remove_file(&path);

        match save_to_path(&source, &path, &namer(100), &no_live_code) {
            Ok(census) => assert_eq!(census.methods_written, 2),
            Err(e) => {
                assert!(false, "save failed: {e}");
                return;
            }
        }

        let target = ProfileStore::new();
        match load_from_path(&target, &path, &resolver(9000), &live_len) {
            Ok(census) => {
                assert!(!census.file_missing);
                assert_eq!(census.methods_seeded, 2);
            }
            Err(e) => assert!(false, "load failed: {e}"),
        }
        assert_eq!(
            view_without_invocations(&source, 100),
            view_without_invocations(&target, 9000),
            "every map must survive a real file round trip under new class ids",
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Round 13 wave 3 (lane replay2; R13-6): a save replaces an existing
    /// file whole (through a renamed temporary) and leaves no temporary
    /// behind, so a second writer's save can only replace the first one's.
    #[test]
    fn a_save_replaces_the_file_whole_and_leaves_no_temporary() {
        let source = populated(100);
        let mut path = std::env::temp_dir();
        path.push("cratonvm-profile-store-atomic-20260928.bin");
        // Longer than any encoding: a direct overwrite would not shrink it
        // any differently, but a stale tail must not survive either way.
        if std::fs::write(&path, vec![0xA5u8; 1 << 16]).is_err() {
            return;
        }
        if let Err(e) = save_to_path(&source, &path, &namer(100), &no_live_code) {
            assert!(false, "save failed: {e}");
            return;
        }
        let (bytes, _) = serialize(&source, &namer(100));
        assert_eq!(std::fs::read(&path).ok(), Some(bytes), "exactly one encoding");
        assert!(
            !super::temporary_sibling(&path).is_some_and(|tmp| tmp.exists()),
            "no temporary left"
        );
        let target = ProfileStore::new();
        assert!(load_from_path(&target, &path, &resolver(9000), &live_len).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    /// Round 13 wave 9 (lane replay5): two threads (two embedded VMs dropped
    /// at once) saving to one path write two different temporaries, so
    /// neither rename can publish the other's half-written bytes; one thread
    /// always gets the same name, beside the target in its directory.
    #[test]
    fn concurrent_savers_on_two_threads_write_distinct_temporaries() {
        let path = std::env::temp_dir().join("cratonvm-profile-store-tmpname-20260928.bin");
        let here = super::temporary_sibling(&path);
        assert_eq!(here, super::temporary_sibling(&path), "stable per thread");
        let there = std::thread::spawn({
            let path = path.clone();
            move || super::temporary_sibling(&path)
        })
        .join()
        .ok()
        .flatten();
        assert!(here.is_some() && there.is_some());
        assert_ne!(here, there, "one temporary per writing thread");
        assert_eq!(
            here.as_ref().and_then(|tmp| tmp.parent().map(Path::to_path_buf)),
            path.parent().map(Path::to_path_buf),
            "renamed within one directory"
        );
    }

    /// A truncated file on disk is an `Err`, and the store is still usable.
    #[test]
    fn a_truncated_file_on_disk_is_an_error_and_the_store_survives() {
        let source = populated(100);
        let (bytes, _) = serialize(&source, &namer(100));
        let mut path = std::env::temp_dir();
        path.push("cratonvm-profile-store-truncated-20260916.bin");
        let cut = bytes.len() / 2;
        if std::fs::write(&path, &bytes[..cut]).is_err() {
            return;
        }
        let target = ProfileStore::new();
        assert!(
            load_from_path(&target, &path, &resolver(100), &live_len).is_err(),
            "a truncated file must be refused",
        );
        let _ = std::fs::remove_file(&path);
    }
}
