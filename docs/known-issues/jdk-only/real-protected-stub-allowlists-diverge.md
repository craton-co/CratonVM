# The real-protected-stub class allow-list exists twice and the two copies are **not** identical — one includes `java/util/StringJoiner`, the other deliberately omits it

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
the divergence is intentional, undocumented at one of the two sites, and means
the cold and warm dispatch paths make different decisions for the same class.**
Wave 2 must **reconcile** these, not assume they are copies of each other.

## What is wrong

"Which `NativeKind::SyntheticStub` natives must yield to loaded real bytecode"
is answered by a hard-coded class allow-list. There are two of them.

### Copy A — `vm/src/vm/vm_exec.rs`, inside `invoke_or_native` (~line 13235)

```rust
let real_protected_stub = synthetic_stub_native
    && (crate::runtime::env_cache::real_bytecode_selector().prefers_real(effective_class)
        || matches!(
            effective_class,
            "java/util/concurrent/locks/ReentrantLock"
                | "java/util/concurrent/LinkedBlockingDeque"
                | "java/util/concurrent/atomic/AtomicBoolean"
                | "java/util/EnumSet"
                | "java/time/Instant"
                | "java/time/ZonedDateTime"
                | "java/util/StringJoiner"          // <-- present
                | "java/io/FileInputStream"
                | "java/lang/ref/Cleaner"
                | "java/lang/ref/Cleaner$Cleanable"
                | "java/lang/management/ManagementFactory"
        ));
```

**11 classes.**

### Copy B — `vm/src/runtime/interpreter/invoke.rs`, `real_protected_stub_class` (~line 10558)

Same eleven, minus `java/util/StringJoiner`, which is replaced in place by a
21-line comment explaining the omission:

> NOT `java/util/StringJoiner` (2026-07-10): yielding this class's SyntheticStub
> natives to real bytecode here exposes a deterministic heap-reference-integrity
> defect (the `gen_heap::read_slot` "corrupt Value cell"/HIB-CV-32 guard fires
> reading `StringJoiner`'s own `size`/`elts` fields back after a `putfield`, on
> the SECOND `add()` call onward) that does not reproduce for an equivalent
> user-defined class with the identical bytecode shape and field count/layout
> (ruled out via a standalone MicroProbe repro) — something specific to this
> being a natively-registered bootstrap class, not the bytecode pattern itself.
> … Path 2 (`invoke_or_native` in `vm/src/vm/vm_exec.rs`) still protects
> `StringJoiner` via its own, separate, long-standing allowlist — this only
> reverts the NEW path-1 (interpreter `try_stackless_invoke`) preference added
> here, back to the proven-safe pre-existing behavior.

**10 classes.**

So: on a vtable *miss*, `StringJoiner`'s synthetic natives yield to real
bytecode. On the interpreter's stackless/cached path, they do not. That is a
deliberate, load-bearing asymmetry — and **Copy A carries no comment saying so.**
A reader who finds Copy A first has no way to know Copy B exists, let alone
differs.

The referenced write-up is
`docs/internal/fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`.
Note the in-code comment still points at the pre-move path
`docs/known-issues/stringjoiner-synthetic-native-real-jdk-field-mismatch.md`,
which no longer exists — a stale reference worth fixing in the same change.

## Not two copies — one list, one derived list, and three inline predicates

The class list has two copies. The *predicate* built on it has more:

* `synthetic_stub_should_yield_to_real_bytecode` (`invoke.rs` ~10516) — the full
  helper: `kind == SyntheticStub` → `real_protected_stub_class(class)` →
  class loaded and `!is_synthetic_stub` → `find_method_recursive` →
  `!m.is_native() && m.code().is_some()`.
* `invoke_or_native` (`vm_exec.rs` ~13235–13270) — the same five steps written
  out inline against Copy A.
* `try_stackless_invoke`'s direct-native step (`invoke.rs` ~11645) — inlined,
  calls Copy B for the class term.
* `populate_invoke_cache` (`invoke.rs` ~12501) — inlined, calls Copy B; the
  helper's own doc comment explains why (*"cannot call the full helper while
  holding the class-manager read lock"*).
* The `VirtualNative` cache-hit path (`invoke.rs` ~21919) — calls Copy B as a
  cheap pre-filter, then the full helper. See
  [cached invoke targets drop the `NativeKind`](cached-invoke-targets-drop-the-nativekind.md).

Both class lists are additionally OR-ed with
`env_cache::real_bytecode_selector().prefers_real(class)`, i.e. the
`CRATONVM_REAL` env selection, so the *effective* set is
(env selection) ∪ (hard-coded list) — and the hard-coded halves differ.

## Why it was not fixed in wave 1

Contract §7 step 3 makes the list unnecessary in principle: *"concrete bytecode
beats a registered `Bridge` or `SyntheticStub`"* is the rule, so the correct
implementation consults `NativeKind` + `Method::code()` and needs no class
names at all. But that only works once every native carries an honest kind — see
[`NativeKind` is ambient and defaults to `SyntheticStub`](native-kind-is-ambient-and-defaults-to-syntheticstub.md)
— and once the `StringJoiner` heap-integrity defect underlying the Copy B
omission is actually fixed rather than routed around. Neither was in wave-1
scope.

## What specifically must change

1. **Do not merge the lists.** Reconcile them: for each of the 11 classes,
   decide the single correct answer and prove it. `StringJoiner` is the one
   known disagreement; the other ten have never been checked for *agreement in
   practice*, only assumed equal.
2. Fix the `StringJoiner` heap-reference-integrity defect (HIB-CV-32 family:
   `gen_heap::read_slot` "corrupt Value cell" firing on the second `add()`),
   which is what forces the asymmetry. Until then, a merged list is a choice
   between reintroducing a known crash and reverting a known fix.
3. Once every SyntheticStub native carries an honest kind, delete both lists and
   the five inline predicates, and let `resolve_dispatch` decide from
   `NativeKind` + `Method::code()`. The wave-1 marker on Copy A states the same
   plan and adds: *"the two must die together."*

## How to verify a fix

* **Divergence test (cheap, do this first):** a unit test asserting
  `real_protected_stub_class(c)` agrees with Copy A's `matches!` for every class
  either one names. It fails today on `java/util/StringJoiner` — that is the
  point; freeze it as a documented expected failure or make it pass by
  reconciling.
* **`StringJoiner` correctness:** the standalone MicroProbe repro referenced in
  the Copy B comment, plus a `StringJoiner` whose second and subsequent `add()`
  calls must be observable in `toString()`. A `StringJoiner` that renders only
  prefix+suffix is the layout-drift symptom; see
  [fabricated object layouts leak into native code](fabricated-object-layouts-leak-into-native-code.md).
* **After deletion:** the `--jdk-only` census must show no
  `native-shadows-bytecode` violations for the 11 classes, and the full
  regression suite must be unchanged in `Compatible` mode.

## Blast radius if done wrong

* **Adding `StringJoiner` to Copy B** (naive "make them match") reintroduces a
  deterministic heap-corruption guard trip on the second `add()` — the exact
  regression the 2026-07-10 change reverted.
* **Removing `StringJoiner` from Copy A** to match Copy B hands `StringJoiner`
  back to synthetic natives on the cold path, whose 5-field fake layout against
  the real 7-field class makes `add()` a silent no-op (documented in
  `native-api/src/registry.rs`'s `drop_real_layout_synthetic` note). Silently
  empty joins, no error.
* **Deleting both lists before the kinds are honest** hands every mis-tagged
  bridge back to bytecode that may not exist.

Neither direction is safe as a mechanical edit. This item requires a decision
per class, not a merge.
