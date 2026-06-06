# jit-api review

Crate under review: `C:\Projects\CratonVM\jit-api` (workspace member
`cratonvm-jit-api`, version 0.3.0). Confirmed there is **no separate
top-level `jit-api/` directory at the workspace root** — the matches
returned by recursive globs are all under `.claude/worktrees/...`
copies, not separate crates. The single real crate is reviewed below.

Reviewed files (every file in the crate):
- `jit-api/Cargo.toml`
- `jit-api/README.md`
- `jit-api/src/lib.rs`
- `jit-api/src/gpu_lowering.rs`

Adjacent crates peeked at for context only:
- `vm/src/jit/helpers.rs` (`build_helpers`, helper bodies)
- `jit/src/x64.rs` (inline-TLAB emission site, helper-call gating)
- `jit/src/lib.rs` (re-export of API types; location of
  `JitInvokeInfo`, `JitMICSlot`, `JitPICSlot` — they live in `jit`, not
  in `jit-api`)
- `jit-cuda/Cargo.toml` (only consumer that would build the
  `gpu-lowering` feature, but does not enable it)

## Summary

- **HIGH** — `JitRuntimeHelpers::validate` has a tautological dead loop
  for the two optional helpers (`get_current_thread`,
  `tlab_post_init`). They are silently *excluded* from both `validate`
  and `null_pointers`, so a corrupted optional pointer slips through
  the "single place to tighten the optional-helper contract later"
  hook described in the docstring (`jit-api/src/lib.rs:198-204`,
  `:244-279`). Soundness implication: a corrupt non-zero
  `get_current_thread` is JIT-CALL'd unconditionally on the inline
  TLAB fast path (`jit/src/x64.rs:7100`), so a silent bad pointer here
  jumps into arbitrary memory inside a JIT frame.
- **HIGH** — `NUM_FIELDS = 33` is hand-maintained with only the
  *length* of two parallel arrays type-pinned. The struct has 38
  fields total; the maintenance contract is human-only and the TODO
  to macro it has sat since round 9
  (`jit-api/src/lib.rs:161, :222-243, :240-243`). Field-list drift
  here = silent loss of `validate()` coverage on whatever the author
  forgets to add — exactly the failure mode the constant is supposed
  to prevent.
- **MED** — `JitRuntimeHelpers` is `#[repr(C)]` (correct, the
  doc explicitly states this is the ABI boundary the JIT bakes
  absolute CALL targets from) but exposes **no `#[repr(C)]`
  invariants test, no field-offset golden test, and no
  `size_of::<JitRuntimeHelpers>()` assertion**. Layout drift between
  the API and `jit/src/x64.rs::emit_call_absolute(self.helpers.foo)`
  call sites would be caught only at runtime by a SIGSEGV.
- **MED** — `CachedBytecodeMethod` (`jit-api/src/lib.rs:23-36`) is a
  public struct of pure `pub` fields with no `Debug`/`PartialEq`,
  invariant docs, or constructor. It’s the central interpreter ⇄ JIT
  data type — easy to mis-construct, and `num_params > max_locals`,
  `code.len() > u32::MAX`, or zero `max_stack` for a non-empty body
  would all happily flow through.
- **LOW** — Crate is OSS-ready for the in-repo (publish=false)
  workflow: SPDX headers on every `.rs` file, copyright correct,
  workspace metadata inherited cleanly, README accurate. No
  third-party deps beyond two in-tree crates. Single non-blocking
  nit: `keywords`/`categories` are inherited from the workspace and
  reuse the umbrella `["jvm","java","virtual-machine",
  "interpreter","jit"]` set even though this crate is the **API**, not
  the JIT itself.

---

## 1. Code review

### Bugs

**HIGH — `validate()` optional-helper loop is a tautological no-op.**
`jit-api/src/lib.rs:198-204`:

```rust
for &opt in &[self.get_current_thread, self.tlab_post_init] {
    if opt != 0 {
        continue;
    }
}
```

This loop has no side effect, no `return false` branch, and is dead
code in both arms. The docstring at `:174-181` claims:

> When either is set to a non-zero value it is an active `CALL` target,
> so it is held to the same non-null contract — this catches a corrupt
> non-zero address silently passing validation.

It does nothing of the sort. Right now `validate()` ⇔ "all 33
mandatory pointers non-zero". The whole purpose of the second loop is
documented but not implemented. Fix: either delete the loop and update
the docstring, or implement the address-range sanity check the
docstring promises.

**HIGH — `null_pointers()` silently excludes optional and offset
fields** (`jit-api/src/lib.rs:208-218, :244-279`). Same root cause as
above: `all_pointers()` is a 33-element array, but the struct has 38
fields. A consumer doing
`if !helpers.validate() { log::error!("nulls: {:?}", helpers.null_pointers()); }`
will be unable to diagnose a misconfigured optional helper because the
diagnostic surface lies. The two-method API gives a misleading
impression of completeness.

**MED — `NUM_FIELDS = 33` is *not* compiler-enforced against the
struct.** The docstring is admirably honest about this
(`jit-api/src/lib.rs:222-243`), but the consequence is real:
`JitRuntimeHelpers` has been extended at least twice (round-7 added
`satb_pre_write_barrier`, round-8 added FMA, round-9 added inline-TLAB
wiring) and only by manual diligence has `all_pointers` /
`field_names` / `NUM_FIELDS` stayed in sync. The macro-generated
field list mentioned in the round-9 TODO is the right fix; no other
mechanism makes the lengths match the struct.

**MED — `CachedBytecodeMethod` has no invariant documentation or
construction discipline** (`jit-api/src/lib.rs:23-36`). Every field is
`pub`. There is no statement of:
- whether `num_params <= max_locals` (it must be, JLS §4.10.2.4
  enforces it via verification),
- whether `code` length is bounded by `u32::MAX` (it must be; bytecode
  uses `u32` PCs in some attributes and `u16` instruction-offsets in
  others),
- whether `max_stack == 0` is legal for a non-empty `code`,
- whether `exception_table` PCs must be in-range against `code.len()`.

This crate is supposed to be "everything needed to JIT a method
without re-locking class metadata" — those invariants would be useful
to land here. Today they're maintained ad-hoc at construction sites
in `vm` and `classloading`.

**MED — `LoweringError::Internal(String)` allocates on every error
path** (`jit-api/src/gpu_lowering.rs:34`). The PTX path is GPU-side
hot-warm, but `Internal(String)` forces a heap allocation per
unsupported opcode and discards the underlying error chain
(no `#[from]`, no `source()` impl on the variants — only the trivial
`std::error::Error` blanket). A `Cow<'static, str>` or `&'static str`
variant for the common "internal: unsupported X" case would avoid the
allocation and match the existing `Unsupported(&'static str)` style.

**LOW — `LoweringError` derives `Debug` but not `Clone`, `PartialEq`,
or `Eq`** (`jit-api/src/gpu_lowering.rs:30`). Implementations can't
easily compare or memoize results. Given it's `#[non_exhaustive]`
deliberately to keep variants addable, `Debug` alone is defensible —
flagging this as LOW only.

### Vulnerabilities — soundness around the unsafe trait boundary

This crate has **no `unsafe`** itself — but it defines the contract
(`#[repr(C)] JitRuntimeHelpers`) on which `unsafe` code in `jit` and
`vm` relies. Risk is concentrated in three places:

1. **`#[repr(C)]` layout drift (HIGH).** `jit/src/x64.rs:64` imports
   `JitRuntimeHelpers` and emits absolute `CALL` immediates via
   `self.helpers.foo` field reads — pure Rust field access, so the
   compiler picks the offset, no fragility there. BUT some call
   sites (e.g. the `emit_call_absolute(self.helpers.uncommon_trap)`
   pattern at `jit/src/x64.rs:836, 2694`, etc.) bake the *value* of
   the helper pointer as a 64-bit immediate in machine code. A field
   re-order in `jit-api` would not break the build but would change
   which helper is called from which generated CALL site, because the
   build script does not regenerate. There is no compile-time
   golden-offset test pinning the layout. Recommendation:

   ```rust
   #[test]
   fn jit_runtime_helpers_field_offsets_pinned() {
       use std::mem::offset_of;
       assert_eq!(offset_of!(JitRuntimeHelpers, newarray), 0);
       assert_eq!(offset_of!(JitRuntimeHelpers, new_object), 8);
       // ... full list, hand-maintained alongside x64.rs
   }
   ```
   This makes a reorder fail the test suite, not silently produce
   wrong machine code.

2. **Optional-helper null check.** `jit/src/x64.rs:15711-15715`
   correctly gates inline TLAB emission on
   `get_current_thread != 0 && tlab_post_init != 0`, so the
   "set to `0` means not wired" contract documented at
   `jit-api/src/lib.rs:137-139, 146` is honored *for the inline TLAB
   site*. But it is honored only because the jit code remembered to
   check; nothing in this crate enforces that consumers must check
   before calling. A future call site that forgets the gate would
   emit a `CALL 0` and crash. Adding helper methods like
   `fn get_current_thread_opt(&self) -> Option<usize>` would shift
   the contract into the type system.

3. **ROP gadget surface from leaked addresses.** `JitRuntimeHelpers`
   is a flat table of *absolute* function pointers baked into RWX
   pages of the code cache. If an attacker can read any one of those
   helper addresses (e.g. via an info-leak in the interpreter), they
   gain the load address of `vm`'s helper functions, which gives
   them a base for libc-style ROP. This isn't a flaw of jit-api per
   se — it inherits the JIT's W^X policy — but the comment on the
   struct (`jit-api/src/lib.rs:38-47`) does not mention that this
   table is effectively the JIT's PLT and should be treated as a
   sensitive asset. Documentation gap → MED.

### Stubs / TODO / FIXME

- `jit-api/src/lib.rs:240-243` — `TODO(round-9): macro this so the
  field list lives in exactly one place`. Genuine actionable TODO,
  multiple rounds old.
- `jit-api/src/lib.rs:330-337` — Audit comment about a deleted
  `JitRuntimeHelpersBuilder`. Informational, not a stub.
- `jit-api/src/gpu_lowering.rs:27-29` — Audit comment marking
  `#[non_exhaustive]`. Informational.
- No `todo!()`, `unimplemented!()`, `unreachable!()`, or `panic!()`
  in this crate. No `unwrap()`/`expect()` either. Clean.

### Performance

- `null_pointers()` returns `Vec<&'static str>` — a heap allocation
  on every call. Used only for diagnostic logging, so MED at worst.
  Could return `impl Iterator<Item = &'static str>` for zero-alloc.
- `all_pointers()` and `field_names()` rebuild their arrays on each
  call. Both are LLVM-inline-trivial and likely vanish in release
  builds, but if `validate()` is ever called per-method (it
  shouldn't be — `vm/src/jit/helpers.rs::build_helpers` is called
  once at startup) the array literals would matter. Not currently a
  hot path → LOW.
- `CachedBytecodeMethod` is `Clone` but not `Copy`; each clone is
  exactly six `Arc::clone`s + 8 bytes of scalar copy → cheap.
  However, `Clone` cloning all six `Arc`s atomically on every method
  re-entry is non-zero — if this type ends up in a per-call path,
  it would matter. Today the consumer site is per-compile, not
  per-call → LOW.

---

## 2. Tests

### Existing coverage

`jit-api/src/lib.rs:339-692` contains 18 unit tests, all inline in
`mod tests`. By category:

| Category | Tests | Notes |
| --- | --- | --- |
| `CachedBytecodeMethod` field round-trip | 9 | construction, clone, Arc-sharing (good), source-file Some/None, exception table 0/1/3 entries, large `code` |
| `JitRuntimeHelpers` field round-trip | 5 | construction, copy, clone, distinct pointers, all-zero, field-access |
| `JitRuntimeHelpers::validate` | 2 | one happy, one with single zeroed field |
| `JitRuntimeHelpers::null_pointers` | 2 | empty when valid; reports two zeroed fields |

Coverage estimate by lines exercised: **~85%** of `src/lib.rs`
non-test code. The dead-loop validation path at `:198-204` is
exercised but, being a no-op, its "correctness" assertion is
vacuous.

`gpu_lowering.rs` has **zero tests**. There is no test of
`LoweringError`'s `Display` impl, no smoke test that the trait is
object-safe (i.e. `Arc<dyn GpuLowering>` compiles), no test of
`LoweredKernel` round-trip.

### Gaps and additions

**HIGH-priority additions**:

1. **`#[repr(C)]` layout pin** — `jit_runtime_helpers_field_offsets_pinned`
   (using `std::mem::offset_of!`, stable since Rust 1.77 which is our
   MSRV). Catches the layout-drift soundness risk.
2. **`null_pointers` covers optional helpers** — once the bug in
   `validate()` is fixed, add a test that zeroing `get_current_thread`
   on an otherwise-valid table reports it. Currently impossible to
   write because the bug hides the case.
3. **`validate()` rejects a corrupt optional helper** — once the
   dead loop is replaced with a real check, the test set should
   include "set `tlab_post_init` to `usize::MAX`" (or any
   non-canonical address) and assert it's flagged.
4. **`CachedBytecodeMethod` invariant assertions** — proptest
   verifying that `num_params <= max_locals`, `exception_table`
   PCs in `0..code.len()`, etc. These invariants are nowhere
   documented or tested; turning them into a `validate()` method
   on `CachedBytecodeMethod` would let the JIT skip its own
   re-checks.

**MED-priority additions**:

5. **`GpuLowering` object-safety smoke test**:
   ```rust
   fn _is_object_safe(_: &dyn GpuLowering) {}
   ```
   Compile-time guard; protects against accidentally adding a
   `Self: Sized` method.
6. **`LoweringError::Display` text format** — current strings
   `"unsupported: ..."` and `"internal error: ..."` are part of the
   public API by virtue of `Display`. Pin them.
7. **Helper-table size check** — `assert_eq!(size_of::<JitRuntimeHelpers>(), 38 * 8)`
   on x86-64 host. Catches accidental `bool` field insertions or
   alignment surprises.
8. **`null_pointers()` field-name ordering** — add a test that
   verifies `field_names()[i]` corresponds to the same field as
   `all_pointers()[i]`. Today this is enforced by manual review only.

**LOW**:
9. proptest for `CachedBytecodeMethod` `Clone`/`Arc::ptr_eq` over
   random inputs — current tests use a fixed fixture.
10. Bench `validate()` to confirm it's not on a per-call path
   anywhere upstream (it shouldn't be; a regression here would be
   measurable).
11. `cargo doc --features gpu-lowering` smoke compile in CI — the
   feature is declared but no consumer crate enables it (see §4),
   so it can rot.

No proptest, fuzz, or `loom` test exists. Given the crate is pure
types + a trait, proptest is the natural fit. Fuzz is unnecessary
here.

---

## 3. Documentation

### Existing

- **Crate-level rustdoc** (`jit-api/src/lib.rs:1-10`) — brief,
  enumerates the three public items, mentions the
  `gpu-lowering` feature. Adequate.
- **README** (`jit-api/README.md`) — clear scope/non-goals/usage,
  realistic "Pre-1.0" status, links workspace. Good for an internal
  crate.
- **Public-API rustdoc on the unsafe-trait boundary** —
  `JitRuntimeHelpers` (`:38-47`) explicitly justifies `#[repr(C)]`
  with the absolute-CALL rationale. Strong. Per-field docs on the
  inline-TLAB triple (`:109-146`) are exemplary — every immediate
  the JIT bakes is cross-referenced to its emission site.
- **`validate()` docstring** (`:163-186`) — careful and frank about
  what it does and does not check (function-pointer alignment, byte
  offsets, etc.). One unintended consequence: it claims to cover the
  optional helpers but does not.
- **`all_pointers()` maintenance contract** (`:222-243`) — honest
  about the human-only enforcement.
- **`gpu_lowering` module** — well-scoped, mentions today's sole
  implementor, links the design doc.

### Missing / weak

- **No `#![warn(missing_docs)]` on the crate.** Several public
  items have either no docs (`LoweredKernel` only one-line, no
  field docs) or thin docs (`LoweringError::Unsupported`,
  `LoweringError::Internal`).
- **`CachedBytecodeMethod` has only a one-line module-level docstring
  and no per-field documentation.** All 12 `pub` fields are
  undocumented — including non-obvious ones like
  `is_synchronized` (does this affect frame layout? the lock-acquire
  is in the interpreter, but does the JIT need it for the monitor
  prologue?) and `num_params` (does this include the receiver for
  instance methods, like `Method.parameter_count` in HotSpot, or
  not?). Add per-field docs.
- **Trait boundary security** — the `JitRuntimeHelpers` docstring
  should note that the table is effectively the JIT's PLT, leaks of
  any field disclose VM load address, and the page that ends up
  holding it (the code cache, not this struct) is the W^X-policed
  surface. One paragraph on the security model would close the
  documentation gap mentioned in §1.
- **Trait object-safety not asserted** in docs — readers writing a
  second `GpuLowering` implementor would benefit from an explicit
  statement.
- **The `gpu-lowering` feature has no consumer.** No crate in the
  workspace enables it (grepped all Cargo.toml). The trait
  effectively dead-codes unless explicitly built. Either delete the
  feature gate and always expose the module, or add a CI job that
  builds with `--features gpu-lowering` to keep it healthy.
- **No CHANGELOG or per-crate version notes.** The version is
  workspace-inherited; the deletion of `JitRuntimeHelpersBuilder`
  (a public API removal) is noted only in an in-file audit comment
  at `:330-337`, not anywhere a downstream reader could see.

---

## 4. OSS readiness

### Cargo.toml (`jit-api/Cargo.toml`)

- `publish = false` inherited from workspace — correct for the
  current "not a stable ABI for third-party JIT back-ends" posture
  documented in the README.
- `license = "Apache-2.0"` inherited correctly.
- `description = "JIT compiler API types for CratonVM"` — accurate
  and complete.
- `readme = "README.md"` — present, accurate.
- `repository` / `keywords` / `categories` inherited from workspace.
  Categories `["compilers","emulators"]` and keywords inherited from
  workspace are the umbrella values intended for the top-level VM
  crate, not strictly the "API types" crate. Non-blocking since
  `publish = false`; a future publishable version should narrow them.
- `[features] gpu-lowering = []` declared, no consumer enables it
  in-tree. Either wire it on in `jit-cuda`'s default features (since
  `jit-cuda` is the sole implementor target) or remove the gate.
- Dependencies: only two — `cratonvm-types` and `cratonvm-reader`,
  both in-tree. **Zero third-party crates.** Excellent for an API
  crate: minimum blast radius, no supply-chain surface.
- `[lints] workspace = true` — inherited correctly.
- No `[badges]`, but `publish = false` makes that moot.

### License headers

Both `.rs` files carry:

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
```

Consistent with the rest of the workspace. README ends with the
correct license + copyright block. No NOTICE file needed in the
crate (the workspace-root NOTICE is sufficient under Apache-2.0
§4(d) for an in-repo crate).

### Blockers

**None.** This crate is publish-ready *technically* but
intentionally held back by `publish = false`, which the README
explains (the surface tracks the VM 1-to-1, not a stable
third-party ABI).

If the team ever wanted to publish:
1. Tighten `keywords` and `categories` to API-types-appropriate
   values.
2. Add `#![warn(missing_docs)]` and fill in the field-level
   documentation gaps in `CachedBytecodeMethod`.
3. Decide whether `gpu-lowering` is a permanent feature gate
   (document why) or a tech-debt item (remove).
4. Fix the `validate()` dead loop before exposing the API to
   third-party JIT back-ends — a tautological public method is a
   bad first impression.

---

## Top 5 fix priorities

1. **HIGH — Fix `JitRuntimeHelpers::validate()` and `null_pointers()`
   to actually cover the optional helpers.** The current loop at
   `jit-api/src/lib.rs:198-204` is dead code, and `null_pointers()`
   silently drops the optional + offset fields. Either delete the
   doc claim or implement the check (the docstring promises the
   latter; a simple `if opt != 0 && !is_plausible_addr(opt) { return false; }`
   with a single canary-range sanity check would satisfy it).

2. **HIGH — Pin the `#[repr(C)]` layout with a golden-offset test.**
   Add a test using `std::mem::offset_of!` (stable on MSRV 1.77)
   that asserts every field offset. This is the only mechanical
   guard against a reorder silently changing which helper a baked
   absolute `CALL` reaches.

3. **HIGH — Replace the hand-maintained `NUM_FIELDS`/`all_pointers`/
   `field_names` triple with a `macro_rules!`-generated declaration.**
   Resolves the round-9 TODO at `:240-243`. Makes the field count
   compiler-enforced and eliminates the human-only maintenance
   contract.

4. **MED — Document `CachedBytecodeMethod`'s invariants and add
   per-field docs.** `num_params` semantics (includes receiver?
   doubles/longs counted as 2?), `max_stack`/`max_locals` lower
   bounds, exception-table PC range against `code.len()`, behavior
   when `is_synchronized && is_static`. A `validate(&self) -> Result<(), Reason>`
   method would let the JIT skip its own checks and would be
   easily proptest-covered.

5. **MED — Add a documentation paragraph on the security model of
   `JitRuntimeHelpers`** — it is the JIT's PLT, contains absolute
   VM addresses, and is the seam through which ROP gadgets become
   reachable if any entry leaks. The docstring at
   `jit-api/src/lib.rs:38-47` should explicitly say so, and any
   future "diagnostic dump" helpers must avoid printing addresses.

(Optional follow-ups, not in the top 5: zero-alloc `null_pointers()`,
`#![warn(missing_docs)]`, decide the `gpu-lowering` feature's
permanence, narrow `keywords`/`categories` if publish is ever
flipped.)
