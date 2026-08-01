# Every JIT ban is GONE — explicit inventory (2026-07-31)

**The bans were first commented out, then removed outright.**
`vm/src/jit/skip_list.rs` (4596 lines) is **deleted**, along with its module
declaration, the six call sites that consulted it, and the types that existed
only to feed them (`SkipPolicy`, `SkipReason`, `InitComplexity`,
`classify_init_complexity`, `allow_packages_from_env`). Four further ban mirrors
in the `jit` crate went with it — see *The four jit-crate mirrors* below. There
is no longer any static ban list anywhere in the VM.

This doc is the record of *what* was removed and *what each one protected
against*, so a regression that appears after this change can be matched to a ban
without archaeology.

**Almost none of this was re-verified as unnecessary.** Exactly one of the 19
skip-list entries (`org/h2/`) was measured. The rest — and all four mirrors —
were removed on request, not because they were shown to be stale. Treat every
entry below as a live suspect when triaging a new JIT failure.

## The 19 skip-list bans, in file order

| # | Target | What it protected against |
|---|---|---|
| 1 | `<clinit>`, non-trivial | A1.4 policy: `<clinit>` re-entrancy during constant-pool resolution. Had an `InitComplexity::Trivial` carve-out. |
| 2 | `<init>`, non-trivial | A1.4 policy: interaction with field-initialisation order. Same carve-out. |
| 3 | `java/util/Collections.indexedBinarySearch` | ES812 postings residual: dispatches a lambda `apply` through the **wrong receiver** after a `java/util` promotion — invokeinterface PIC not invalidated on a changing lambda receiver. |
| 4 | `java/util/stream/MatchOps.make{Int,Ref,Long,Double}` | Not correctness — **performance**. Every call reaches an internal indy that lowers to an always-deopt uncommon trap: 6928 `UnreachedCode`/`MakeNotCompilable` events for `makeInt` alone in one ES run, none of which stopped re-invocation. |
| 5 | `<clinit>` (no complexity info) | As #1, on the path with no classification. |
| 6 | `<init>` (no complexity info) | As #2. |
| 7 | interface default methods | A1.4: known regalloc parameter-mapping bug. |
| 8 | any method on an **unnamed thread** | Early-init safety: thread-local JIT state may not be set up yet. |
| 9 | `java.lang.reflect.Proxy` subclasses | **Design, not a defect.** The body is a `super.h.invoke(this, mN, args)` trampoline whose semantics CratonVM overrides *at dispatch*, not in the bytecode. Compiling it runs the wrong semantics. |
| 10 | `java/math/MutableBigInteger` | Confirmed JIT-only array-index corruption in divide/normalisation. |
| 11 | `java/math/BigInteger` | Whole-class; a multi-method interaction, never narrowed. **Note:** the removed SUNEC-INTPOLY ban depended on this staying active — re-test P-384/P-521 EC keygen+sign+verify (`EcIntPolyProbe.java`). |
| 12 | ANTLR `PredictionContext` equality/hash | Null-`PredictionContext` parse corruption. Survived the `groovyjarjarantlr4/` package lift on purpose. |
| 13 | `org/h2/` | **The one measured entry.** See below. |
| 14 | `org/apache/commons/logging/` | Confirmed corruption during per-class logger wiring; never narrowed past "some method in the package". Was deliberately **not** liftable — a confirmed corruption should not have a casual opt-out. |
| 15 | unconditional hash-miscompile cluster | Conservative-policy only. |
| 16 | AQS family | Conservative-policy only. |
| 17 | `KeyedReentrantReadWriteLock$LockImpl.lambda$lock$0` | One Tomcat lambda shaped `v == null ? new X() : v` — branch, allocate-and-construct on one arm, pass through on the other, merge, return. Plausibly the callee-saved-clobber family, but this exact shape (a lambda) was not covered by that fix. |
| 18 | ConcurrentLinkedQueue family | Conservative-policy only. |
| 19 | `org/jboss/modules/` | Tag-bit corruption in allocate-then-putfield-heavy module-graph traversal (W2-CHM / RBC.1 / SPB.1-7 archetype). |

## The four jit-crate mirrors (also removed)

These lived in `jit/src/lib.rs` and `jit/src/tiered.rs`, consulted at
`try_compile`'s final admission gate — the **second** of the two gates that
enforce a ban. Removing a `skip_list.rs` entry without removing its mirror does
not lift the ban; that trap has caught this codebase before, which is why they
went too.

| Target | What it protected against |
|---|---|
| `org/hsqldb/` | The Flyway HSQLDB integration **SIGSEGVs** under JIT; the package-level interpreted control completes the class. Was present **twice** at the same gate. |
| `com/sun/org/apache/xerces/internal/` | Corrupts `SchemaGrammar`'s `SymbolHash` during Hazelcast XML schema validation. |
| `org/yaml/snakeyaml/emitter/Emitter.emit` | Called "the exact proven corruptor" — ES-JIT-DEOPT-GC.1. |
| `java/math/BigInteger` (`tiered::is_biginteger_arithmetic_jit_denied`) | The jit-crate half of #10–11. Also fed `TierSignals::class_denied`, now pinned `false`. |

**None of these four had a "lift once X is fixed" note.** Unlike `org/h2/`, they
were live crash reports rather than stale policy, and nothing here establishes
that the underlying defects are fixed. If HSQLDB, Xerces schema validation, YAML
emission or BigInteger arithmetic starts failing, suspect these first.

Removed with them, as they existed only to make those package bans liftable:
`jit_allow_package`, `jit_allow_packages_filter`, `jit_allow_entry_allows_prefix`,
and the `CRATONVM_JIT_ALLOW_PACKAGES` / `CRATONVM_JIT_PUTFIELD_INIT` flag
registrations.

## The one remaining lever

`CRATONVM_JIT_DENY` is now the **single** force-interpret mechanism. It absorbed
`CRATONVM_JIT_BISECT_SKIP` (deleted), which did strictly less: both match
`Class.method`, but `DENY` matches substrings, so it covers single methods and
whole packages.

```bash
CRATONVM_JIT_DENY='org/h2/mvstore/MVStore.commit'    # one method
CRATONVM_JIT_DENY='org/hsqldb/,org/yaml/snakeyaml/'  # restore a package ban
```

`CRATONVM_JIT_BISECT_ONLY` survives as the inverse allowlist ("only these
prefixes stay JIT-eligible") — a question a deny-list cannot express. It moved
from `skip_list.rs` to `jit/src/lib.rs`. Both are applied in `try_compile`, the
final gate, so nothing routes around them.

The remaining JIT give-up mechanisms are the ones HotSpot also relies on:
deopt-driven `MakeNotCompilable`, structural compiler bailouts, and the
code-cache cap. What is gone is the static, curated identity list.

## The one that was measured

`org/h2/` (#13) is the only entry with fresh evidence. Its own comment said
*"LIFT THIS BAN once [the TreeMap comparator bug] is fixed"*, and that blocker
is fixed:

* `TreeMapCmpProbe` — the witness for a JIT-compiled `new TreeMap<>(cmp)`
  losing its comparator — passes **40000/40000, four consecutive runs**.
* `org.h2.test.jdbc.TestMetaData`, recorded as the **sole** regression from
  lifting and caused entirely by that bug through `SelectGroups.reset()`'s
  `new TreeMap<>(session)`, passes **3/3** with the ban lifted (was FAIL 3/3).

A fresh 218-class A/B reached 112/218 per arm before being stopped, with the
picture stable throughout and **zero PASS → non-PASS regressions**:

| | banned | lifted |
|---|--:|--:|
| PASS | 88 | **89** |
| FAIL | 10 | **7** |
| HANG | 14 | 16 |

Per-class deltas: `TestCompatibility` FAIL → PASS; `TestMultiThread` and
`TestGetGeneratedKeys` FAIL → HANG (both already failing and already tracked
elsewhere — they now burn the 300 s timeout instead of failing fast, which
costs suite wall-time but is not a correctness regression).

## Tests

21 tests asserted a now-removed ban. They are `#[ignore]`d rather than deleted
or inverted: the assertions are the record of what each ban covered.
Twenty are in the deleted `skip_list.rs`'s place (they went with the file); the
survivor is `tiered::tests::hibernate_biginteger_divide_cluster_is_never_background_enqueued`,
which asserts behaviour rather than naming a predicate.

## Restoring

Any of these comes back through `CRATONVM_JIT_DENY` with no rebuild, which is
the right first move when triaging. Restoring one in code means recovering it
from git history — this doc plus the commits that removed it are the index.
