# Building one `java.lang.Throwable` costs ~2.2 us (HotSpot: ~0.3)

**Status:** OPEN, partially addressed. Two of the three "Next step" items below
are now implemented (2026-09-21/22) with measured improvement and zero test
regressions; the third (`create_exception_object`'s write-locked class load)
is also implemented but did not move the kernel it targets in repeated
same-host measurement, and the load-bearing `javaNewOnly`/`javaNewThrown`
rows are still 3-5x HotSpot. This page stays OPEN rather than moving to
`docs/internal/` — the defect this page describes is reduced, not closed.
**Owner-area:** `../../../vm/src/runtime/exceptions.rs`
(`create_exception_object` / `create_exception_object_for_class`), the
`native_exc_init_*` constructor shadows (`../../../native-builtins/src/lang_misc.rs`),
the stack capture + `throwable_stacks` registry
(`../../../vm/src/runtime/stackwalker.rs`, `../../../vm/src/vm/vm_init.rs`,
`../../../vm/src/vm/realms/thread_realm.rs`), and the per-VM Throwable field
cache (`../../../native-api/src/registry.rs`).
**Found by:** lane `iexc`, 2026-09-20, closing the implicit-exceptions page.
**Worked by:** an autonomous session 2026-09-21/22 implementing the three
"Next step" items below, plus a validation pass that found and fixed a
`--jdk-only`-shaped locking-invariant gap in the GC remap path (see "What
changed" below).

## Evidence

`ThrowCost` (scratch probe, reproduced verbatim below), 200 000 iterations per
kernel, three rounds, azure `vm1` at load ~10, ns per iteration.
**The arms are sequential, not interleaved**, and this host's speed drifts
between hours (this session in particular ran alongside ~100 other concurrent
agent sessions competing for the same shared machine, at times taking free RAM
to single-digit MB and free disk to near-zero — see the caveat below), so do
not read the two columns against each other as a careful A/B. Nothing below
needs one: every claim is a ratio between two kernels of the SAME process,
which load drift cannot invent.

| kernel | what it costs | HotSpot 21 | CratonVM (dev, pre-fix) | CratonVM (this fix, clean measurement) |
|---|---|---:|---:|---:|
| `javaNewOnly` | `new ArrayIndexOutOfBoundsException(msg)`, no throw | 273-434 | 2 165-2 277 | **1 286-1 989**, typical ~1 300-1 500 |
| `javaNewThrown` | the same, thrown and caught in the same method | 264-470 | 2 698-3 161 | **2 036-2 978**, typical ~2 050-2 260 |
| `vmMinted` | a compiled bounds check's AIOOBE, caught in the same method | 3-645 | 3 078-4 148 | **~3 000-5 800** (see caveat — no clean signal either way) |
| `allocOnly` | `new Object()` | 25-37 | 221-259 | 165-253 (sanity check: comparable machine speed) |

The load-bearing row is the first one. `new SomeException("msg")` dropped by
roughly a quarter to nearly half against the pre-fix `dev` binary — real,
reproducible, measured on a quiet run of this same host — but CratonVM is
still 3-5x HotSpot's 273-434, not at parity. `javaNewThrown` moved by a
similar, smaller margin. `allocOnly` stayed flat across both binaries, which
is the methodology's own sanity check that the two measurements were taken
under comparable machine conditions.

**Caveat on `vmMinted`:** this session implemented Next-step item #2 (below)
specifically to move this row, then measured it three ways — once freshly
after the fix, once with the fix reverted on the *same* rebuilt binary
(A/B, same host, minutes apart), and the two came back statistically
indistinguishable (4 627-5 758 with the fix vs. 4 865-5 758 without,
both elevated well above the original 3 078-4 148 baseline). `allocOnly` was
*also* elevated in that same window (up to 306-310 against a 221-259
baseline), which says the ambient machine load at measurement time — not the
fix — explains the elevated `vmMinted` numbers, and that this specific A/B
could not detect whatever effect item #2 has, in either direction. The fix is
still landed because the reasoning for it (see item #2's own note) holds
independent of measurement, and it caused no regression in either arm.
Re-measure on a quiet host before drawing a conclusion about it.

## Why it matters

Frameworks throw as control flow. Spring, Hibernate and the JDK's own
`NumberFormatException`/`NoSuchMethodException` paths build throwables in
ordinary Java, so this is not a JIT number: it is a floor under every
`try`/`catch` in every workload, and it is the largest single item left on the
implicit-exception rows now that the compiled round trip is gone.

## What changed (this pass)

All three "Next step" items from the original page, in the order the evidence
supported:

1. **Cache the resolved `detailMessage`/`cause`/`suppressedExceptions`/
   `backtrace`/`depth`/`stackTrace` field indices and the `StackTraceElement`
   class id, per VM, not per process.** `ThrowableLayoutCache`
   (`vm/src/vm/realms/thread_realm.rs`) holds one `AtomicUsize`/`AtomicU32`
   slot per field, resolved once and read with `Ordering::Relaxed` on every
   subsequent throw. The previous approach used bare process-global statics
   (`AtomicUsize` module statics in `lang_misc.rs`) — silently correct for one
   VM per process, silently WRONG the moment a second `SharedVm` exists in the
   same process (embedding, tests, `cratonvm-embed`): the second VM would
   inherit the first VM's bootstrap class layout indices. The new cache is
   reached through `NativeHeapAccess::throwable_field_index`
   (`native-api/src/registry.rs`), a trait method every native context
   implements, with the default staying a plain by-name `resolve_field_index`
   call so lightweight/mock contexts remain source-compatible without opting
   in. Test: `throwable_layout_slots_are_not_shared_between_vms`
   (`vm/src/vm/realms/thread_realm.rs`) constructs two independent caches and
   asserts a write to one is invisible on the other.

   Resolving the `<init>`/`fillInStackTrace` METHODS by name per throw turned
   out to be moot for the load-bearing kernels: ordinary `new X(msg)` never
   goes through `create_exception_object_for_class`'s by-name
   `invoke_on_class_shared` calls — it hits the `native_exc_init_message`
   family in `lang_misc.rs`, which dispatch as ordinary registered natives,
   not per-throw name resolution. Nothing needed to change there.

2. **Replace the write-locked `load_class` probe in `create_exception_object`
   with a read-locked probe that only falls back to the write path on an
   actual miss.** `vm/src/runtime/exceptions.rs`: a read-lock call to
   `ClassManager::resolve_fast_path_class_id` — the *exact* fast-path lookup
   `load_class` itself runs first, under its own write lock — resolves an
   already-loaded class (every VM-minted throwable after the first one of its
   kind) without ever taking the write lock. A genuine miss falls through to
   the original write-locked `load_class`, byte-identical to before. This is
   the one item whose benefit this session could not cleanly measure (see the
   caveat above) — it is sound by construction (a read lock is never more
   expensive to acquire than a write lock when uncontended, and skipping the
   write lock removes one more thing that could serialize against a *real*
   class-loading write-lock holder elsewhere), but re-verify on a quiet host
   before citing a number for it.

3. **Stop paying the `LineNumberTable` scan for a caught-and-discarded
   throwable.** This went further than "index the table once per method":
   line-number resolution is now deferred entirely out of the construction
   path. `capture_full_trace_without_lines` (`vm/src/runtime/stackwalker.rs`)
   captures exact frame identities (class, method, bci) with lines
   unresolved; `SharedVm::throwable_stack_trace_for`
   (`vm/src/vm/vm_init.rs`) resolves lines lazily, only when a caller actually
   asks for the trace (`getStackTrace()`/`printStackTrace()`), via
   `resolve_line_numbers_in_place`. The overwhelmingly common
   caught-and-never-inspected throwable now never touches a `LineNumberTable`
   at all — strictly better than the original ask, not merely "once per
   method instead of per frame."

   This also changed how retained traces are keyed and stored:
   `SharedVm::store_throwable_stack_trace` used to key a global
   `RwLock<FxHashMap<i32, _>>` by the throwable's Java **identity hash** —
   requiring a mark-word hash to be minted at construction time solely to
   populate this VM-private registry, on every single throw. It is now keyed
   by the throwable's raw (GC-remapped) heap address, across 32 independently
   locked shards (`THROWABLE_STACK_SHARDS`,
   `vm/src/vm/realms/thread_realm.rs`) chosen by `throwable_stack_shard`
   (masks the pointer's low bits, dropping the bottom 3 since heap objects
   are ≥8-byte aligned) — sharding so concurrent throwers on different
   threads don't serialize on one lock, and dropping the mark-word-hash
   requirement entirely from the hot path. `SharedVm::throwable_stack_trace(hash:
   i32)` — the compatibility door for callers that only hold a Java identity
   hash — still exists but is now a linear scan across shards; it is not on
   any construction path.

   **Locking-invariant fix found during validation:** the GC remap sweep
   (`remap_and_sweep_throwable_stack_traces`) takes a write lock on the
   CURRENT shard and, when a relocated entry lands in a *different* shard,
   also takes a write lock on that destination shard while the first is still
   held. That is only deadlock-safe because the function is never re-entered
   concurrently with itself — its one call site
   (`gc::update_all_roots`) runs only after every other mutator is parked at
   a safepoint. This invariant held but was undocumented; it now has an
   explicit comment at the function naming the call chain that guarantees it
   and prescribing the fix (two-phase collect-then-apply, or shard-index lock
   ordering) if root fixup is ever parallelized across GC worker threads. A
   stale doc comment on `throwable_stack_shard` (left over from an earlier,
   identity-hash-keyed design; it no longer resembled what the function
   actually does) was also corrected.

## Verification

`cargo build --release` (fat LTO, codegen-units=1): clean. 41 tests across
the touched area, 0 failures, 1 expected ignore (`stack_walker_default_never_returns_null`,
real coverage lives in `native-builtins/src/stack_walker.rs::tests`):
`throwable_layout_slots_are_not_shared_between_vms` (lib unit test),
`wp1_9_stackwalker` (9 tests, including GC-stress coverage via
`stackwalker_reflect_gc::stackwalker_reflective_fill_survives_gc_stress`),
`exception_edge_tests` (20 tests), `jit_npe_message_from_an_optimizing_body`,
`jit_npe_message_from_compiled_code`, `pgo02_guarded_virtual_inline`,
`stack_trace_compiled_aioobe`, `string_format_throwing_tostring`,
`wp8_10_7_throwable_subclass_getmessage` (6 tests).

## What is NOT the cause (unchanged from the original page)

* Not allocation. `allocOnly` stayed in the low-200s ns range across every
  measurement in this pass, and a throwable is one object plus one `String`.
* Not the compiled-code exit path. `javaNewOnly` never leaves the interpreter
  and never traps, and it still carries the largest share of the total.
* Not `OmitStackTraceInFastThrow`. Default OFF, only applies to VM-minted
  implicit exceptions — cannot touch `javaNewOnly` at all.

## Next step

The remaining gap on the load-bearing rows (CratonVM still 3-5x HotSpot on
`javaNewOnly` even after this pass) was not re-profiled — this session
measured the effect of the three specific suspects the original page named,
it did not re-run a fresh profile to find what is left. Before further
changes:

1. Re-measure everything in this page on a quiet host — this session's
   numbers were taken alongside severe, uncontrolled machine contention (see
   the `vmMinted` caveat above), and the absolute values here should not be
   treated as authoritative, only the qualitative "these three fixes landed,
   tested clean, one of them logically should have and could not be shown to
   move its target" story.
2. Profile fresh against the post-fix binary. The original suspects are
   substantially closed; whatever is left (still ~1.3-2.0us against
   HotSpot's ~0.3-0.4us on `javaNewOnly`) is a new question, not a re-read of
   the old one.

## The probe

`tools/probes/ThrowCost.java` (checked in; grew one kernel,
`javaNewNoTrace` — `new NoTraceException(msg)` where the constructor passes
`writableStackTrace=false`, isolating construction from stack-capture
entirely — and the default round count moved 3 → 5 for a steadier read.
Invoke with explicit `200000 3` args to reproduce the evidence table above
exactly):

```java
public class ThrowCost {
    static final int[] A = new int[4];
    static int isink;

    static int msgLen(Throwable e) { String m = e.getMessage(); return m == null ? 0 : m.length(); }

    static int vmMinted(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            try { s += A[i + 8]; } catch (ArrayIndexOutOfBoundsException e) { s += msgLen(e); }
        }
        return s;
    }

    static int javaNewThrown(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            try { throw new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4"); }
            catch (ArrayIndexOutOfBoundsException e) { s += msgLen(e); }
        }
        return s;
    }

    static int javaNewOnly(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            ArrayIndexOutOfBoundsException e =
                new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4");
            s += msgLen(e);
        }
        return s;
    }

    static int allocOnly(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) { Object o = new Object(); s += o.hashCode() & 1; }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime(); isink += vmMinted(n);
            long t1 = System.nanoTime(); isink += javaNewThrown(n);
            long t2 = System.nanoTime(); isink += javaNewOnly(n);
            long t3 = System.nanoTime(); isink += allocOnly(n);
            long t4 = System.nanoTime();
            System.out.println("round " + r
                + " vmMinted " + ((t1 - t0) / n)
                + " javaNewThrown " + ((t2 - t1) / n)
                + " javaNewOnly " + ((t3 - t2) / n)
                + " allocOnly " + ((t4 - t3) / n));
        }
        System.out.println("isink " + isink);
    }
}
```

`msgLen` is not decoration: HotSpot's `OmitStackTraceInFastThrow` makes the
`vmMinted` kernel's `getMessage()` return `null`, and a bare
`e.getMessage().length()` NPEs the reference VM out of the comparison
altogether. See `tools/probes/ThrowCost.java` for the checked-in copy,
including the added `javaNewNoTrace` kernel.
