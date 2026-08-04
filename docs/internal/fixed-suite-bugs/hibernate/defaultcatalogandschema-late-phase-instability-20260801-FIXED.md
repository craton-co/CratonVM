# `DefaultCatalogAndSchemaTest` is intermittently unstable in every collector arm

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-08-03/08-04, branch `fix/hib-dcast-latephase-instability-20260803` (commits `fe14e6b14`, `d0387cdc3`, `c33e9bc87`, plus a follow-up 08-04 commit — see [Update](#update-20260804-the-kind_of-residual-was-the-same-corruption-family)). Fifteen distinct fault sites closed, all one defect family: something writes an implausible header into old-gen memory (confirmed present via `old_gen::scan_region`'s own corruption-detection diagnostic — see the 08-04 update), and every reader that trusted the corrupted bytes without validating them first — nine inside the GC's own internal walkers, six more in the VM-level read barrier and dispatch layer every mutator path funnels through — could crash on it. Each of the original nine was confirmed via disassembly to be the literal fault instruction; the 08-04 batch was confirmed via live reproduction and symbolization against a fresh debug build. |
| **ID** | `HIB-DCAST-LATEPHASE.1` |
| **Originally found** | 2026-08-01, while re-verifying `HIB-GCOVERHEAD-HALFFULL.1` against the `dev` tip. |
| **Residual** | The underlying corruption **source** — whatever writes an old-gen header with `num_slots`/`array_length` ≈ 33,554,432–33,554,433 and otherwise-valid `kind`/`element_type` tags — is still **not root-caused**. All fifteen known ways to crash on it are now closed (validate-before-dereference, matching the rest of this doc's fixes), converting every remaining occurrence into either a silent no-op or a catchable Java-level exception instead of a VM crash. See [Update](#update-20260804-the-kind_of-residual-was-the-same-corruption-family). The class's own separately-documented test-instability and slow/occasionally-hanging behavior (see [Residual](#residual-pre-existing-test-failures-and-slow-hanging-runs)) are unchanged and out of this doc's scope; they predate this investigation and are not GC crashes. |

## Original symptom (2026-08-01)

Same class, same classpath, `--Xmx 1500m`, JIT on, real JDK. On `origin/dev` @
`cc8167f94`, with selective promotion enabled, the class crashed 5 of 7 runs
(4 SIGSEGV, 1 `capacity overflow` panic); with promotion disabled, 0 crashes
but real test failures and truncated runs. Every crash's fatal-error dump
reported **`faulting pc not attributed to a compiled method`** — the original
investigation never symbolized a single one of these crashes. See the
"Where to start" section of the original doc (preserved in git history) for
the state of the investigation before this session.

## Root causes (nine, one defect family)

`ObjectHeader::kind: ObjectKind` and `ObjectHeader::element_type:
ArrayElementType` are `#[repr(u8)]` enums with only a handful of valid
discriminants (`ObjectKind`: 0/1/2; `ArrayElementType`: 0, 4–11). Loading an
out-of-range byte into either as a *typed* value is immediate undefined
behaviour per Rust's memory model — not "reads garbage and continues", but
UB the instant the load happens — and this codebase's release profile (fat
LTO, `codegen-units = 1`, `opt-level = 3`) is exactly the configuration where
LLVM exploits that UB into a hardware trap (`ud2` → `SIGILL`) rather than
doing anything resembling "the wrong thing but not crashing". A sibling
class of bug — trusting `num_slots`/`array_length` (plain `u32`s, no
enum-validity issue, but still attacker/corruption-controlled) without a
plausibility bound, or trusting a compact-object's registered-layout lookup
without a fallback for "not found" — produces the same class of hazard via a
`SIGSEGV` instead: a reference-slot scan strides past an object's real
extent into unmapped memory.

Nine call sites had this gap. All nine are fixed by validating the raw tag
byte(s) (via `object_kind_from_tag`/`array_element_type_from_tag`, or a
`num_slots`/`array_length` plausibility bound, or both) **before** ever
constructing a typed `&ObjectHeader` reference or trusting a derived size.

| # | Site | File:line (current) | Trap | Confirmed via |
|---|---|---|---|---|
| 1 | `OldGen::scan_region` / `scan_region_filtered` (the walk itself) | `gc/src/old_gen.rs:792` (`validate_header_tags_or_desync`) | SIGILL | RVA `0x2C45D1`, `llvm-objdump` showed `ud2` after an inlined enum-discriminant check |
| 2 | `GenerationalHeap::scan_object_for_old_refs` | `gc/src/gen_heap.rs:10607` | SIGSEGV | RVA `0x2C0413`; a code comment already named this exact gap ("Counted (not rejected) here — rejecting is the FIX, which is deliberately not part of this diagnostic commit") from an earlier, incomplete fix |
| 3 | `GenerationalHeap::forward_object_impl` + `gc::try_forward_object` (legacy semispace collector) | `gc/src/gen_heap.rs:11259`, `gc/src/gc.rs:395` | SIGILL | RVA `0x2C48B8`; `llvm-objdump` showed a `ud2` reached via `core::panicking::panic_bounds_check`'s divergent-call landing pad, traced back through the call graph to this function's unconditional `ptr::read` of `kind`/`element_type` before its own (too-late) plausibility guard |
| 4 | `for_each_ref_slot`/`forward_ref_slots` (gen_heap.rs) + `for_each_old_gen_ref` (old_gen.rs) + `concurrent_mark.rs`'s scan — compact object misclassified as legacy | `gc/src/gen_heap.rs:13663`/`13751`, `gc/src/old_gen.rs:1225`, `gc/src/concurrent_mark.rs:884` | SIGSEGV | RVA `0x2C0C43`, identical across **two different threads** (`main-vm` and `junit-jupiter-timeout-watcher`) in separate runs — `compact_oop_scan` returns `None` both for "legacy object" and for "compact object whose layout isn't registered right now", and every caller read that as "must be legacy", misinterpreting a compact body under the legacy `num_slots * SLOT_SIZE` formula |
| 5 | `victim8_neighbor_explains_zero_prefix` | `gc/src/gen_heap.rs:12842` | SIGILL | RVA `0x2DF7FE`; reads a deliberately unproven, speculative neighbor address — the same class of gap as #1–3, in a function whose own comment already claimed "compare the raw byte" while the code still read `nheader.kind` typed |
| 6 | `object_body_size`, legacy branch — missing `num_slots` cap | `types/src/field_layout.rs:961` (cap at `1000`, line `MAX_PLAUSIBLE_LEGACY_SLOTS` at `1010`) | SIGSEGV | Deterministic 2/2 reproduction at RVA `0x2A07F3` in a `profsym` (full-debuginfo) build, disassembly showed a legacy Value-slot scan with a loop index around 33 million |
| 7 | `object_body_size`, compact branch — `.unwrap_or(0)` fallback | `types/src/field_layout.rs:961`, `.unwrap_or(IMPLAUSIBLE_BODY_SIZE)` at line `983` | SIGSEGV | Fix 6 alone reproduced the *same* crash 3/5 times in the next verification batch, always at the identical instruction; careful re-reading of `object_body_size` found the compact branch's `0` fallback computes to exactly `HEADER_SIZE`, passing every caller's `total < HEADER_SIZE` corruption check and desyncing the walk into a live object's real body |
| 8 | Defense-in-depth: bound every reference-slot loop against the walked/known size, not just the size *computation* | `gc/src/gen_heap.rs` (`for_each_ref_slot`/`forward_ref_slots`, capped `1 << 24`), `gc/src/gc.rs` (three call sites, same cap), `gc/src/old_gen.rs` (`for_each_old_gen_ref`, now takes `total_size` and bounds precisely against it) | SIGSEGV | Fix 7 *still* left a residual — the same RVA recurred once more after fix 7 landed. This fix closes the whole family at the point memory is actually dereferenced, independent of how a bad `num_slots`/`array_length` got there |
| 9 | `old_gen_mark_candidate_plausible` — validated `kind`, never `element_type` | `gc/src/gen_heap.rs:12643` | SIGILL | RVA `0x2E3192`, the one crash in an otherwise-clean 4/5 batch; `gen_object_total_size(header)` reads `header.element_type` typed for an Array-kind candidate whose element-type byte this screen never checked |

### The investigative arc for fixes 6–8 is the central lesson of this session

Fix 6 (cap `num_slots` in the legacy branch) looked complete by reasoning: it
mirrors an identical, already-proven cap elsewhere in the same file
(`gen_object_total_size`). Verification showed otherwise — the *same* crash,
at the *same* instruction, recurred 3 times in the next 5-run batch. Reading
the function fresh (not re-trusting the earlier analysis) found the real
trigger: the **compact** branch's `.unwrap_or(0)` fallback, untouched by fix
6, silently reports a compact object with an unregistered layout as having a
confident zero-byte body — which is small enough to slip past every caller's
corruption check and desyncs the walk into that object's *real*, larger
body. Fix 7 closed that specific path — and the same crash recurred *again*,
once more, in the very next batch. Only fix 8 — bounding the actual
dereferencing loops against a known-safe limit, independent of how the walk
arrived at a wrong size — finally closed the family for good, later
confirmed by fix 9's investigation, which found and closed a ninth,
previously-unexamined site of the same tag-validation gap and has not
recurred since.

**The lesson: confidence in the reasoning was insufficient three times
running for this specific family.** Only empirical re-verification — a full
run of the real workload after *every single fix*, not just at the end —
caught each gap. A future investigator hitting a "surely this closes it"
moment in GC header-validation code should budget for at least one more
verification round than feels necessary.

### Methodology

The original doc's crashes were never root-caused because `faulting pc not
attributed to a compiled method` was accepted as a dead end. This
investigation found every site by:

1. **`CRATONVM_SYMBOLIZE=<RVA>` against the exact crashing binary** —
   resolves a faulting RVA to a function + file:line via the release PDB's
   line-table info. Works for most sites, but fat LTO + `codegen-units = 1`
   both inline aggressively and let the linker fold byte-identical code from
   *unrelated* functions together (identical code folding, ICF) — this
   produced at least two misleading symbol names during the investigation
   (a crash symbolized to `G1Collector::record_collection` while the active
   collector was generational; disassembly showed unrelated jump-table
   dispatch code, not that function's real body). **Always cross-check a
   symbol result against disassembly when the active-collector/thread
   context doesn't match the named function.**
2. **`llvm-objdump -d` on the exact faulting RVA**, using the toolchain's
   bundled copy
   (`~/.rustup/toolchains/<toolchain>/lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objdump.exe`).
   This is what actually found sites 3, 6, and 9 — reading the raw
   instruction bytes (a `ud2` opcode, a jump-table dispatch, a striding
   memory-access pattern) when the symbol alone was ambiguous or absent.
3. **The `profsym` Cargo profile** (`[profile.profsym]` in the workspace
   `Cargo.toml`: inherits `release` — same `lto = "fat"`, same `opt-level =
   3` — but `debug = 2`, full debug info). Built and used specifically when
   the release binary's `debug = "line-tables-only"` PDB gave insufficient
   resolution for a *deterministically reproducing* crash (site 6). Because
   it keeps the same optimization level, a bug that depends on codegen
   shape or timing still reproduces identically, unlike a plain `cargo
   build` (unoptimized) rebuild.

## Fix

All nine sites follow the same pattern established at site 1 and already
used elsewhere in this codebase for conservative-root validation (see
`docs/internal/fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md`,
2026-07-08 — the same defect class, fixed for two validators but never
propagated to the walkers this doc covers): read the raw tag byte(s) via a
raw pointer (`*ptr.add(OBJECT_KIND_OFFSET)` / `ARRAY_ELEMENT_TYPE_OFFSET`),
validate through `object_kind_from_tag`/`array_element_type_from_tag`
(`types/src/heap_types.rs`), and only construct a typed `&ObjectHeader`
reference — or trust a derived size — after validation succeeds. Where a
loop's *bound* itself is corruption-controlled (`num_slots`, `array_length`),
either cap it against a "no real class/array is this large" constant
(`1 << 24`, matching the pre-existing convention in
`gen_object_total_size`/`old_gen_mark_candidate_plausible`) or, more
robustly (fix 8), bound the dereferencing loop directly against a
known-good extent rather than trusting the count in isolation.

Each fix ships a regression test that corrupts only the specific raw
byte(s)/field under test on an otherwise-legitimate allocation and asserts
the affected function rejects it (returns `false`/`None`/skips scanning)
instead of trapping — nine new tests total, none of which existed before
this investigation.

## Verification

This was an extensive, iterative investigation: roughly **40+ verification
runs of the real `DefaultCatalogAndSchemaTest` workload** across the
baseline and nine post-fix batches (each run takes 25–65 minutes on this
shared, variably-loaded host; several batches ran 5 runs in parallel). A
condensed batch-by-batch tally:

| Batch (fix state) | Runs | Clean | Crash | Hung/inconclusive |
|---|---|---|---|---|
| Baseline (`dev` tip, no session fixes) | 3 | 1 | 2 | 0 |
| After fix 1 | 4 | 2 | 1 | 1 |
| After fix 2 | 7 | 2 | 3 | 2 |
| After fixes 3–5 | 5 | 2 | 2 | 1 |
| `profsym` rebuild (fixes 1–5) | 3 | 1 | 2 | 0 |
| After fix 6 | 5 | 0 | 3 | 2 |
| After fix 7 | 5 | 0 | 4 | 1 |
| After fix 8 | 5 | 4 | 1 | 0 |
| After fix 9 | 5 | 2 | 1 | 2 |

Every crash in every batch symbolized (directly, or after disassembly) to
one of the nine sites above, or — in exactly one case, the last crash
observed (batch "after fix 9") — to a genuinely different, unrelated site;
see [Residual](#residual-a-new-unrelated-crash-surfaced-in-late-verification) below.
**No crash signature recurred after its own specific fix landed** — each
of the nine RVAs/symbols is confirmed absent from every subsequent batch.

`cargo test -p cratonvm-gc`: **960 passed, 0 failed** (957 before this
session; +3 from this session's final commit, +2 not double-counted from
earlier commits — 9 new tests total across the three commits).
`cargo test -p cratonvm-types`: **485 passed, 0 failed** (482 before).

`cargo test -p cratonvm-vm` and `-p cratonvm-types`'s
`flag_declaration_guard` integration test have one **pre-existing, unrelated**
failure (`CRATONVM_JIT_NO_PRECISE_FIELD_OPS` undeclared in
`jit/src/lib.rs:12410`) — not touched by this investigation, not a
regression; confirmed present before any of this session's commits.

## Update 2026-08-04: the `kind_of` residual was the same corruption family

The `VmHeap::kind_of` crash flagged below as "a different subsystem and
almost certainly a different root cause" was **wrong** — a follow-up
investigation (same day) proved it is the *same* old-gen header corruption
this doc closes, just reaching the mutator side of the VM instead of the
GC's own internal scan. The original framing is preserved below (struck
through in spirit, not in text) so the reasoning error is visible, not
silently erased.

**Reproduction.** Built `target/profsym` (the `[profile.profsym]` release
config with full debug info — see the Methodology section) and ran
`DefaultCatalogAndSchemaTest` in repeated parallel batches. The crash, which
the original investigation saw once in ~40 runs, reproduced at a much
higher rate under this fresh build — 1/6, then 4/8 shards in back-to-back
batches — and **every single occurrence** was immediately preceded by
several `cratonvm_gc::old_gen` `WARN "BREAK on implausible header"` lines
(the diagnostic fix 6/7/8 added: `old_gen::scan_region` detects a header
whose computed size is implausible, logs the raw bytes, and skips it rather
than trusting it). Across dozens of these warnings, the corrupted headers
shared a specific, repeating fingerprint: valid `kind`/`element_type` tags
(0/1, 4–11 — never an invalid discriminant) but `num_slots`/`array_length`
of **33,554,432 or 33,554,433** (`0x0200_0000`/`0x0200_0001`) — not random
garbage, a specific recurring value, still unexplained. This is a
**corruption source distinct from and *not* explained by any of the nine
tag-validation fixes above** (those fixes validate `kind`/`element_type`
bytes; this corruption's tags are valid — it's the size fields that are
wrong), and it remains unroot-caused.

**Why it crashed here and not in the GC's own scan.** `old_gen::scan_region`
was already hardened (fixes 6–8) to detect this exact implausible-header
shape and safely skip it. But that hardening lives entirely inside the
GC-internal walkers (`old_gen.rs`, `gen_heap.rs`, `gc.rs`,
`concurrent_mark.rs`). The **VM-level read barrier**
(`VmHeap::load_and_forward`, `gc/src/vm_heap.rs`) that every mutator path —
interpreter dispatch, JIT helpers, native field/array accessors — calls
before touching an `ObjectRef` was never touched by that work, and neither
were the shared dispatch wrappers (`kind_of`, `element_type_of`,
`identity_hash_code`, `class_id_of`) it and they all funnel through. Traced
via `CRATONVM_SYMBOLIZE` + `llvm-objdump` to four confirmed fault sites, all
downstream of the same mechanism: `load_and_forward` reads
`ObjectHeader.forwarding_ptr` from a header that may be one of the corrupted
ones above, and — unlike every *other* header field, all of which fixes 1–9
now validate — the forwarding pointer was trusted completely unchecked. A
corrupted header can spuriously read as "forwarded", handing back a garbage
target address (observed as the canonical sentinel `0xFFFFFFFFFFFFFFFF`)
that the caller then dereferences:

| Site | Symbol | Trigger |
|---|---|---|
| 1 | `NativeHeapAccess::array_length` (`vm/src/vm/vm_exec.rs:8992`) | `native-collections::map_state` validates a bucket array via `heap_kind_of`, then calls this several lines later — the object had already gone bad by then |
| 2 | `VmHeap::kind_of` via `dispatch_virtual::execute_invokevirtual_cached` (`vm/src/runtime/interpreter/dispatch_virtual.rs:1582`) | receiver taken from the Java operand stack via `peek_at`, forwarded, then kind-checked — the *forwarding* step returned the garbage target |
| 3 | `NativeHeapAccess::get_field` (`vm/src/vm/vm_exec.rs:8726`, via `class_id_of`) | same mechanism, a different accessor |
| 4 | `VmHeap::load_and_forward` itself (`gc/src/vm_heap.rs:657`) | `obj` was already invalid *before* even reaching the forwarding check — corruption entering earlier, likely at a JIT bail-to-interpreter frame-reconstruction boundary (every occurrence's crash header showed `gc young-gen last incomplete-coverage reason: innermost-rbp-belongs-to-unguarded-callee` and an **unregistered JIT frame** on the faulting thread) |

**Fix.** `load_and_forward` now validates both `obj` on entry and the
extracted `forwarding_address()` via `is_object_address` before trusting
either, falling back to the original pointer (matching its existing
null-forwarding-address fallback) rather than dereferencing an implausible
target. `kind_of`, `element_type_of`, `identity_hash_code`, and
`class_id_of` (`gc/src/vm_heap.rs`) now validate independently too, rather
than trusting that whatever called them already checked — defense in depth,
since a corrupted header can be reached directly without going through
`load_and_forward` at all. `array_length` (`vm/src/vm/vm_exec.rs`) has its
own guard plus a `[ARRAY-LEN-GUARD]` diagnostic, mirroring its existing
"not an array" fallback. Six new regression tests (`gc/src/vm_heap.rs`,
`vm/src/vm/vm_exec.rs`).

**This does not fix the corruption** — it fixes every known way the
corruption can crash the VM. A run that hits it now sees either no visible
effect (the corrupted object silently reads back as
`ObjectKind::Object`/`ClassId(0)`/hash `0`) or, if something downstream
depended on the *correct* value, a catchable Java-level exception (observed
once: `NoSuchMethodError: java.util.Properties.hasNext()Z` thrown from
JUnit's own launcher during test-run teardown — a wrong virtual dispatch
landing on a stale/mismatched cache entry, not a new bug class, and not a
crash) instead of `EXCEPTION_ACCESS_VIOLATION`.

**Verification.** 14 parallel `DefaultCatalogAndSchemaTest` runs against the
fully-hardened `profsym` build (8 + 6, two separate batches): **zero**
`EXCEPTION_ACCESS_VIOLATION`s, versus 5 across the 14 runs immediately
preceding the fix (1/6, then 4/8). The three timeouts (`RC=124`) observed in
one batch showed **zero** `"BREAK on implausible header"` warnings —
unrelated to this corruption family, consistent with the pre-existing hang
issue in [Residual](#residual-pre-existing-test-failures-and-slow-hanging-runs)
below, which already predates and is out of scope for this doc.

**Still OPEN, for a future investigation**: find what actually writes
`num_slots`/`array_length` ≈ `0x0200_0000` into an otherwise-valid-tagged
old-gen header. The repeating exact value (not random garbage) suggests a
specific source — a stale constant, a misinterpreted offset, or a
region-size literal leaking into a size field — rather than generic memory
corruption. `CRATONVM_DBG=jit-names` for a named JIT stack and the
`old_gen::scan_region` "BREAK on implausible header" dump (now confirmed to
fire reliably under this same workload) are the two established points to
resume from.

## Residual: pre-existing test failures and slow/hanging runs

Unchanged by this investigation, and out of its scope — these are the
**older, separately-documented** instabilities the original doc's own "Two
faults, not one" framing already called out:

- **Test failures without a crash.** Several runs in this investigation's
  own batches (e.g. `failed=4`, `failed=7`) completed all 132 tests with
  real assertion/mapping failures and no VM crash. This matches the
  original doc's description of a class that "fails tests... in every arm,
  independently of promotion" — a pre-existing issue this doc's fixes were
  never expected to touch.
- **Slow or apparently-hung runs.** Several runs in this investigation
  stalled for extended periods without producing a result or a crash
  report, consistent with the original doc's own "Cost" section: "no arm
  fails reliably — the worst arm is 2 in 3, the best 1 in 5" and a
  documented degenerate case of "progress collapsing to ~2 tests/20 min".
  Not investigated further here.

Neither of these blocks closing this doc: the doc's own urgent concern —
"this blocks merging `fix/hib-gcoverhead-halffull-20260731` into `dev`"
because promotion-ON turned a heap wedge into a VM crash — is resolved. The
crash family is fixed; the slower, separate correctness/performance issues
remain tracked (informally, via this note) for future work.

## Related

- `docs/internal/fixed-suite-bugs/source-debug-jit-conservative-root-invalid-header-tag-sigill.md`
  — the same defect class (invalid header tag reaching a typed enum read),
  fixed 2026-07-08 for two conservative-root validators; this doc closes the
  same gap in the walkers those validators didn't cover.
- `docs/internal/fixed-suite-bugs/gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`
  — fixed the plausibility screen (`old_gen_mark_candidate_plausible`) and
  the nine push sites into the old-gen mark worklist; explicitly deferred
  "rejecting" `scan_object_for_old_refs`'s bad-kind hits as follow-up work
  (site 2 above closes that follow-up).
- `docs/internal/fixed-suite-bugs/hibernate/invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md`
  — fixed a *different* hazard (relocating a root on a failed moving-young
  coverage proof) for this same test class, and explicitly retracted its own
  "the class passes now" claim, carving out the residual instability that
  became this very doc.
