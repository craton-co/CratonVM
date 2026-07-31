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

Worktree `C:\craton\CratonVM-bytebuddy-retire-20260730`, branch
`codex/fix-bytebuddy-retire-20260730` merged up to `origin/dev`, binary
`C:\craton\bb-retire-20260730\cratonvm-bbretire-r11.exe`
(SHA-256 `69D2E62D530992D374AC2CE3AA87D832731128194E1680506182C69E3ABF5CDA`),
JDK 25.0.3, manifest `crash302.txt` (the exact 302 classes from the 2026-07-28
no-ban crash run), 6 shards, 900 s per-class cap, no
`CRATONVM_JIT_ALLOW_PACKAGES` override.

### The 302-class ByteBuddy corpus

| Mode | Classes | PASS | FAIL | CRASH | HANG | ABORTED | found | started | ok | skipped |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| JIT | 302 | 301 | 0 | 0 | 0 | 1 | 1281 | 1275 | 1272 | 6 |
| `--nojit` | 302 | 301 | 0 | 0 | 0 | 1 | 1281 | 1275 | 1272 | 6 |

**Zero crashes and zero hangs in either mode** — the entire reason the ban
existed — and zero failures. The counts are identical between the two modes.

The single ABORTED row in each mode is
`ManyToManyAssociationClassGeneratedIdTest`, the documented flush-queue
assumption abort. It is **not** waived: re-run with an explicit queue type it
passes outright, in both modes:

| Run | JIT | `--nojit` |
|---|---|---|
| 3 `action.queue` classes, `-Dhibernate.flush.queue.type=graph` | 3/3 PASS | 3/3 PASS |
| `ManyToManyAssociationClassGeneratedIdTest`, `-Dhibernate.flush.queue.type=legacy` | 1/1 PASS | 1/1 PASS |

No assumption-only row is counted as green anywhere in this table.

### `ASTParserLoadingTest` — the class that drove the ANTLR work

| Binary | JIT | `--nojit` failures per run |
|---|---|---|
| r3 (before any ANTLR rooting fixes) | 106/106 | 1, 15 |
| r7 (parent/merge fixes) | 106/106, then 1 FAIL under 6-shard load | 2 |
| r8 (index-based config iteration) | 106/106 | 0, 0, 0, 0, 1, 0 |
| r10 / r11 (final, merged with dev's own rooting pass) | 106/106 | 0 in both corpus arms, twice |

### Unit tests

`cargo test --release --no-fail-fast -p cratonvm-native-collections
-p cratonvm-native-builtins` — `gc_native_pins` **12/12**, including the five new
alloc-time relocation tests. Those tests earned their keep during the dev merge:
against dev's independently-written collections rooting pass they dropped to
9/12, which is how the three remaining unrooted iterator constructors
(`make_iterator_from_array`, `native_ad_iterator`, `alloc_unmod_list_itr`) were
found and fixed.

**Zero failures overall.** Six environment/timing tests (`tzdb` ×2,
`proxy_selector` env vars, `StampedLock` ×2, `ForkJoinPool` quiescence) were
failing on the intermediate trees; they are all in files this branch does not
touch, and they went green when the final `dev` merge brought in that team's own
test-triage work — confirming they were never this branch's.

`vm/src/jit/skip_list.rs` carries the permanent gate test described in section 4.

## 6. The `--nojit` HQL mis-parse, and where it ended up

`docs/known-issues/hibernate/antlr-native-roots-moving-young-hql-misparse-20260730.md`
carries the full account. In short: the native ANTLR intrinsics held raw
`ObjectRef` locals — and whole `Vec<ObjectRef>` config snapshots — across
allocating calls, so a moving young collection linked dead addresses into the
parser's own graph, and the poisoned config was then memoized as a DFA edge.
That is why one mis-timed collection broke a whole grammar path
(`<expression> <comparison-op>`) for the rest of the process.

It is **not** the trivial-accessor fast path that the 2026-07-29 investigation
blamed and deleted; that deletion is reverted here, with the measurements that
refute it recorded in section 3.

Two independent efforts converged on this: the fixes on this branch, and a
broader rooting pass another session landed on `dev` while this work was in
flight. The merge takes dev's version of `antlr_intrinsics.rs` wholesale rather
than re-landing a competing rework. The corpus is clean in both modes on the
merged result, but the underlying idiom — a bare `ObjectRef` living across an
allocating call — is still the file's default style, so the known-issue doc
stays OPEN with a recommended scoped-handle approach and
`CRATONVM_NO_MOVING_YOUNG=1` as the interim mitigation.

## 7. Related

- `actionqueue-graph-default-tests-legacy-tradeoff-20260727.md` — the
  `org.hibernate.orm.test.action.queue.*` classes that self-abort under the
  non-GRAPH default; run with an explicit queue type here, not waived.
- `hibernate-atnstate-transitions-npe-intermittent-hql-parse-20260721-FIXED.md`
  — the 2026-07-22 pass over the same ANTLR file for the same defect class.

## 8. RE-VERIFICATION 2026-07-31 — full 4548-class run, same class, same benign abort

A subsequent fresh 4548-class categorize run (not the 302-class ByteBuddy
corpus above) also reports `ManyToManyAssociationClassGeneratedIdTest` as
ABORTED (`apps/hib-suite-runner/analysis/06-full-suite-categorize-20260730/all-4548-classes-status.tsv`,
line 2626), this time with `found=6 started=6 ok=3 failed=0 aborted=3`
(isolated `CratonRunner` re-run against the fresh `CratonVM-hib-local-0712-v3`
binary reproduces this exactly). That is a different count than the single
abort this section reported for the 302-class corpus, but the same root
cause: the class overrides 3 of the 6 inherited
`AbstractManyToManyAssociationClassTest` methods
(`testRemoveAndAddEqualElement`, `testRemoveAndAddEqualCollection`,
`testRemoveAndAddEqualElementNonKeyModified`), and each override's first line
is `skipForGraphQueue(scope)` → `assumeFalse(queueType == QueueType.GRAPH,
...)`. Since the `actionqueue-graph-default-tests-legacy-tradeoff-20260727-FIXED.md`
fix restored Hibernate's upstream GRAPH default, all 3 overridden methods
assume-abort by design under the default queue type; the 3 non-overridden
inherited methods run and pass normally (`ok=3`). Ran the same class through
plain HotSpot (`java.exe`, same JDK, same classpath, no CratonVM in the loop):
identical `found=6 started=6 ok=3 failed=0 aborted=3`. Confirms this
section's original "documented flush-queue assumption abort... not waived"
conclusion still holds against the full-suite run; no doc correction or new
known-issue filed.
