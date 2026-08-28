# L1 — `Unsafe`: 222 rows, the highest-risk lane

**Read `HANDOFF-20260828-SCOPE.md` first.** It has the method, the traps and the
landing protocol. This doc is only what is specific to L1.

**Owner: unclaimed.** Add yourself to the table in the scope doc's §0 when you
take it. L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

## Your families

```text
jdk/internal/misc/Unsafe   120 bridge-with-code rows
sun/misc/Unsafe            102
                           ---
                           222   (10% of the 2244-row surface)
```

Registrars are spread: `native-builtins/src/unsafe_natives_ext.rs` plus
`lib.rs`, `phases_early.rs`, `deprecated_internal.rs`, `classloader.rs` and
`lang_class.rs`. **Run the `owns_slot` dump before touching any of them** — this
family is the most duplicated in the tree.

## Why this lane is different from the other six

Every other lane is about a library contract. This one is about **memory
safety**. A wrong bounds check or a wrong offset here does not throw the wrong
exception — it reads or writes memory belonging to something else, and the
failure surfaces arbitrarily far away.

So "aim at the edges" applies with an extra clause: **a defect you find here may
not be safe to fix in isolation.** Prefer recording a measured finding over a
speculative repair, and say which you did.

## What is already known — read before probing

* `a-null-base-unsafe-access-means-off-heap-not-a-static-field` — FIXED
  2026-08-24. `float`/`double` were the only widths lacking an `_mb` wrapper;
  `int`/`long` were never latent.
* `objectFieldOffset` now rejects a static field (fixed this campaign; the
  `is_static` flag had been destructured and discarded as `_is_static`).
* `no-array-element-cas-but-blocking-regions-exist` — field CAS and `fetch_add`
  exist, `casTabAt` does NOT.
* `arena-handles-escape-to-native-through-any-pointer-returning-jni-call` — the
  SEGV shape, worth knowing before probing `Unsafe` against FFM.
* `cratonvm-atomics-are-lock-based-not-hardware-atomic`.

## Suggested probe shape

One probe, `UnsafeShadowSweep.java`, sections per width. Ask:

* every `get*`/`put*` width against the four base kinds — a heap object field, a
  static field, an array element, an off-heap address;
* `objectFieldOffset` / `staticFieldOffset` / `arrayBaseOffset` /
  `arrayIndexScale` on a normal field, a static, a final, a missing name, a
  primitive class, an interface, an array class, and `null`;
* the CAS family: success, failure, wrong expected type, null object;
* `allocateMemory(0)`, `allocateMemory(-1)`, `freeMemory(0)`, and
  `setMemory`/`copyMemory` with zero and negative lengths;
* `arrayIndexScale` on a NON-array — HotSpot's own refusal here is recorded as
  broken, so measure it rather than assuming either answer.

**Print no address.** They differ per run by construction. Print lengths,
exception types, and round-tripped VALUES.

## Nearest OPEN item

`Arena`/`MemorySegment` report the INTERFACE `java.lang.foreign.MemorySegment`
as an instance's class, requested at `panama.rs:191`. Unclaimed and closest to
this lane; details in
`the-definition-of-done-screen-run-for-the-first-time-20260828.md` §4. It is a
layout question, not a rename — the real `NativeMemorySegmentImpl` has its own
field layout and that module addresses raw slots.
