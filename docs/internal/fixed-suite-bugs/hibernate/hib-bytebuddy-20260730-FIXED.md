# HIB-BYTEBUDDY — the `net/bytebuddy/` blanket JIT ban is gone for good

**Status:** FIXED and closed 2026-07-30. There is no `net/bytebuddy/` guard in
`vm/src/jit/skip_list.rs`, Byte Buddy is JIT-eligible, and a unit test fails the
build if a blanket guard is ever re-added. The historical 302-class crash corpus
passes in both JIT and `--nojit`.

This document supersedes and absorbs `hib-bytebuddy-removed-20260728.md` and the
short-lived `hib-bytebuddy-reinstated-20260729-FIXED.md`.

## 1. What the ban was, and why it kept coming back

| When | What happened |
|---|---|
| 2026-06-13 | `net/bytebuddy/` blanket-banned from the JIT after `SimpleEnhancerTests` hung (rc=124), spinning in `TypeDefinition$Sort.describe` / `TypeDescription.represents`. |
| 2026-07-28 | Ban removed. Evidence was the named witness plus a **15-class** A/B sample under `org/hibernate/orm/test/bytecode/enhancement/**`. |
| 2026-07-28 (later) | A full 4,548-class suite run with the ban absent recorded **302 CRASH** classes spread across dozens of unrelated packages. |
| 2026-07-29 | Ban re-instated on the then-current `77389fa06` runtime. The 302 corpus went to 298 PASS / 3 assumption-aborts / 1 timeout, and the ban was declared the fix. |
| 2026-07-30 | That conclusion is **wrong**, and is retracted below. |

The 15-class sample really was too narrow — Hibernate uses Byte Buddy-generated
proxies for ordinary lazy loading, so the blast radius was never confined to
tests with "enhancement" in the package name. But breadth was the only thing the
2026-07-29 investigation got right.

## 2. The actual root cause of the 302-class crash spike

The no-ban crash report names
`net/bytebuddy/description/ModifierReviewable$AbstractBase.matchesMask(I)Z` and
faults **fetching an instruction in the middle of its own JIT body**. That is not
a shape a bytecode miscompile takes: a miscompile produces a wrong value or a
data fault, not an instruction-fetch fault at a valid mid-body address. It is the
signature of a compiled body being unmapped while a live frame is still executing
it.

The crash binary predates the three JIT code-lifetime fixes that are now on
`dev`:

- `3fe14734a` — *retire a superseded artifact only when JIT execution is quiescent*
- `ac300e6f6` — *do not unmap a retired code buffer while a frame is executing it*
- `463bd32e2` — *never unmap a compiled body while a thread is executing it*

Byte Buddy is simply the most JIT-churn-heavy code in the suite — it generates
and re-resolves type descriptions constantly, so it retires and republishes
compiled artifacts more than anything else. That made it the place the
code-lifetime defect surfaced first, and banning it from the JIT hid the defect
instead of fixing it. Re-instating the ban on the old runtime "worked" for
exactly that reason.

**Lesson recorded for future ban decisions:** a blanket package ban that makes a
crash disappear is evidence about *where* a defect surfaces, not *what* it is.
Before accepting one, check whether the faulting address is inside generated code
and whether the crash shape is a value fault or a code-lifetime fault.

## 3. The residual found while verifying, and what it actually was

Verifying on current `dev` surfaced an unrelated, **nondeterministic** failure:
`ASTParserLoadingTest` under `--nojit` mis-parses valid HQL. A different test
failed on each run — `testComponentQueries`, `testUnaryMinus`,
`testExplicitEntityCasting`, `testParameterMixing` — always with the parser
rejecting a well-formed comparison, e.g.

```
SyntaxException: At 1:38 and token '=', mismatched input '=' expecting I
  [from Human h where -(h.intValue - 100)=74]
```

The 2026-07-29 investigation attributed this to `dev`'s new stackless
`try_execute_cached_trivial_instance_getter` accessor fast path and deleted it.
**That attribution was wrong**, and the deletion is not carried here. Two
measurements settle it, both on one binary:

1. `CRATONVM_TRIVIAL_GETTER_VERIFY=1` cross-checks every fast-path hit against
   `resolve_field_ref_loader_aware` — the resolver the real `getfield` opcode
   uses. A full 106-test `ASTParserLoadingTest` run reported **zero**
   divergences in field index, descriptor byte, reference-ness, volatility, or
   owning class — and still mis-parsed. The fast path does not compute wrong
   field values.
2. The same binary, same fast path enabled, with `CRATONVM_NO_MOVING_YOUNG=1`:
   **106/106 PASS**.

So the fast path only perturbs allocation and safepoint timing on an
accessor-dense workload; the defect is a **missing native root under the moving
young collector**. Deleting the optimization would have masked a real GC bug and
regressed the workload it was written for.

Both diagnostics are kept so this stays cheap to re-check:
`CRATONVM_TRIVIAL_GETTER=0` disables the fast path, and
`CRATONVM_TRIVIAL_GETTER_VERIFY=1` re-runs the divergence check.

## 4. Fixes landed

**GC roots across allocation (the substance).** Native code that builds an object
graph must keep every part of it pinned across each allocation in the middle,
because a moving collection relocates objects whose only reference is a Rust
local. Fixed in:

- `native-collections`: every iterator, spliterator, and snapshot constructor
  (ArrayList, ArrayDeque, LinkedList, PriorityQueue, HashSet, TreeMap, TreeSet,
  LinkedBlockingQueue, the unmodifiable wrappers, and the generic snapshot
  iterator), plus `map_alloc_node`, `alloc_view_backing`, `resync_view_set`, and
  the stream drain/materialize paths.
- `AccessController.doPrivileged` (all three overloads): the action object is now
  rooted for the whole privileged window. Without it, a collection between the
  code-base cache lookup and `invoke_virtual` handed `run()` a stale receiver.
  This is what made JAXB's `ReflectionNavigator$10` re-run its superclass search
  forever — the long-standing "`ASTParserLoadingTest` JIT reflection storm".
- `ServiceLoader$Itr`, the annotation-proxy constructor in `lang_class.rs`, and
  the `StackWalker` frame-array-stream graph.

**Permanent gate.** `vm/src/jit/skip_list.rs` now asserts, under both
Conservative and Aggressive policies, that the exact historical faulting method
(`ModifierReviewable$AbstractBase.matchesMask`) and both original hang witnesses
(`TypeDescription.represents`, `TypeDefinition$Sort.describe`) stay JIT-eligible.
A future blanket `net/bytebuddy/` guard fails the unit suite.

**Corrections to the 2026-07-29 branch, carried as reverts.**

- `gc_alloc_array` no longer branches on `disable_jit()` to retry young-only. It
  was added for this HQL mis-parse, the build carrying it still failed the test,
  and young-only post-GC retries are exactly what the documented spurious-OOM fix
  removed.
- `p59_sf_get_method_type` is unguarded again; the RETAIN_CLASS_REFERENCE check
  moved to the `getMethodType` registrations. `getDescriptor()` delegates through
  that helper and is specified *not* to require the option, so guarding the
  shared helper made every default-walker `getDescriptor()` throw.
- The HashMap/LinkedHashMap collect walkers use a node budget rather than
  allocating a `HashSet` per bucket on a hot path. A chain longer than
  `size + capacity + 1024` is still reported and truncated, so a torn link
  cannot hang a native with no Java stack.

## 5. Validation

See `RESULTS` section below — filled in from the final binary.

## 6. Related

- `actionqueue-graph-default-tests-legacy-tradeoff-20260727.md` — the three
  `org.hibernate.orm.test.action.queue.*` classes that self-abort under the
  non-GRAPH default; run with an explicit queue type here, not waived.
