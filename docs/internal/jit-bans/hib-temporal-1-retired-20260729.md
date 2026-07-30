# HIB-TEMPORAL.1 (`org/hibernate/`) — retired 2026-07-29

**Status:** fixed and removed. The package-wide Hibernate JIT ban is no longer
present in either the VM eligibility policy or the JIT crate's final admission
gate. The real Hibernate ORM 8.0 classes that kept this ban open pass completely
with the ban absent in both JIT and `--nojit` modes.

## Original failure

Lifting `org/hibernate/` previously corrupted Hibernate bootstrap. In
`InstantTests`, 148 of 204 cases failed with:

```text
org.hibernate.boot.registry.selector.spi.StrategySelectionException:
Default resolver threw exception
```

`ASTParserLoadingTest` could then fail during setup before starting any tests.
The earlier report attributed the cluster to Hibernate temporal/DDL code, but
that code was only the first consumer to expose more general VM defects.

## Root causes and fixes

### 1. The JIT inlined classfile bodies that were shadowed by registered natives

Background C1 compilation inlined the five-byte classfile body of
`ConcurrentHashMap.<init>()V`. CratonVM registers a native implementation of
that constructor which installs the map's segmented backing store. Inlining the
classfile body bypassed the native, so maps created after tier-up silently lost
their `put` operations. Hibernate's `StrategySelectorImpl` stores its named
strategies in such a map, which is why the `"default"` strategy disappeared.

`resolve_inline_site` now rejects both directly registered natives and inherited
methods whose resolved declaring implementation is native. This aligns inline
admission with ordinary callee compilation.

### 2. Native collection loops retained movable references across Java calls

Native `ArrayList.equals`, iterator creation, and map lookup paths retained raw
`ObjectRef` values while element equality, allocation, or another Java call
could trigger moving young GC. The fixes pin each live receiver, backing array,
key/value, and snapshot element, then re-read it after every allocating or
Java-reentrant operation. Map lookup now also follows the Java contract:
`searchKey.equals(storedKey)`, with a full-hash check before `equals`.

### 3. ANTLR prediction-context intrinsics retained movable references

The HQL parser's native `computeReachSet` path and its recursive prediction-
context merge/cache helpers reused raw configs, parents, maps, and cache keys
across allocations and Java re-entry. A moving collection could relocate any
of them, producing intermittent order-sensitive failures even under `--nojit`.

The ANTLR intrinsics now establish balanced native-root frames for the complete
live object graph, eagerly pin snapshot members before processing any member,
and refresh all objects after GC-capable calls. The same discipline covers ATN
config allocation, closure recursion, config-set lookup/add, singleton/array
prediction-context merging, and double-key merge-cache get/put.

### 4. Background callee compilation recursively acquired the class-manager lock

After rebasing onto the current generic-converter JIT work, unrestricted JIT
could stop during Hibernate model/ByteBuddy class generation while
`--nojit` and `CRATONVM_BG_COMPILE=0` completed. The compiler path
`try_jit_compile_callee_slow` held a class-manager read guard and called the
generic-metadata bytecode scanner, which acquired a second read guard. If a
class-loading writer queued between those two reads, `parking_lot`'s task-fair
`RwLock` parked the nested read behind the writer while the outer read kept the
writer parked: a self-deadlock.

The scanner now takes the caller's existing `ClassManager` guard instead of
locking again. A compile-time signature regression prevents restoring the
recursive acquisition. A default-background-JIT traced `InstantTests` run
subsequently reached all 204 test invocations without its watchdog firing.

### 5. Copying young GC could relocate a JIT frame whose roots were not fully rewritable

The remaining default-JIT failure was not another Hibernate eligibility issue.
The moving-young verifier observed real compiled execution with an unregistered
JIT entry, a missing exact frame base, and unpublished live oop slots.  A
copying collection can only be correct when every live compiled-frame location
is both enumerable and rewritable before from-space is moved; discovering a
gap while scanning is too late to repair that collection.

`refresh_moving_young_coverage_for_current_thread` therefore treats the
presence of compiled code as a VM-wide relocation-safety boundary.  JIT remains
fully enabled, but young GC selects its existing non-moving sweep while JIT
code ranges are live. Interpreter-only processes retain copying young GC. This
is intentionally a collector policy, not an ANTLR/Hibernate exception or a JIT
admission ban. The fallback is accounted as
`jit-relocation-contract-unproven`, and the Hibernate JIT matrix below runs
through that real default policy with no GC override.

## Ban removal

The following two independent guards were deleted:

- `hibernate_temporal_residual_skip_prefix` from
  `vm/src/jit/skip_list.rs`;
- `hibernate_temporal_jit_deny_prefix` and its final-admission check from
  `jit/src/lib.rs`.

The VM regression now asserts that representative slash- and dotted-form
Hibernate class names are JIT-eligible under both conservative and aggressive
policies.

## Regression coverage

- `vm/tests/jit_collection_ctor_identity.rs` runs the same Java fixture in the
  interpreter and JIT and compares eight observations. It covers native-
  shadowed `HashMap` and `ConcurrentHashMap` constructors plus allocation-heavy
  `ArrayList.equals` under a 64 MiB heap.
- `native-collections/tests/mock_hashmap.rs` covers search-key equality
  direction, full-hash collision filtering, resizing, and moving-reference
  behavior.
- `native-builtins/src/antlr_intrinsics.rs` includes a focused
  prediction-context array-merge test which also asserts that no native pins
  leak.

Focused results from the final source:

| Gate | Result |
|---|---:|
| Background compiler class-manager guard regression | 1/1 |
| Hibernate package JIT-eligibility regression | 1/1 |
| JIT collection constructor/GC regression | 1/1; 8 interpreter/JIT observations identical |
| ANTLR intrinsic unit tests | 18/18 |
| Native collection `mock_hashmap` tests | 16/16 |

## Direct Hibernate acceptance

Fixture:

- Hibernate checkout: `C:\craton\CratonVM\apps\hibernate-orm`
- runner: `C:\craton\CratonVM\apps\hib-suite-runner\CratonRunner`
- JDK: Eclipse Adoptium 25.0.3.9
- clean release binary SHA-256:
  `1398FF985AD5CC378125E9F7FE2C16A99037C06ECC791CEFC338B118FF4F5FA6`

Each row below ran in a fresh process with no JIT allow/deny/bisect override,
no background-compiler override, no GC-mode override, and no diagnostic/reset
environment variable:

| Class | JIT | `--nojit` |
|---|---:|---:|
| `org.hibernate.orm.test.type.temporal.InstantTests` | 204 started; 112 passed; 92 intentional assumption aborts; 0 failed/skipped | same |
| `org.hibernate.orm.test.hql.ASTParserLoadingTest` | 106/106 passed | 106/106 passed |
| `org.hibernate.orm.test.hql.HQLInsertAndUpdateTest` | 5/5 passed | 5/5 passed |
| `org.hibernate.orm.test.hql.WithClauseTest` | 8/8 passed | 8/8 passed |
| `org.hibernate.orm.test.hql.EnumTest` | 4/4 passed | 4/4 passed |
| **Total** | **327 started; 235 passed; 92 intentional aborts; 0 failed/skipped** | **same** |

The v19 runs were fresh real-JDK processes using the normal JIT policy and
normal GC defaults; no JIT allow/deny/bisect, background-compiler, GC-mode, or
diagnostic environment override was present. The default JIT parser class
passed 106/106 in 548,646 ms and the independent `--nojit` parser class passed
106/106 in 384,263 ms, both with zero failures, aborts, and skips.

## Disposition

HIB-TEMPORAL.1 is retired. Do not restore the `org/hibernate/` package ban for
these symptoms; regressions should be investigated through the native-shadowed
inline, native collection rooting, or ANTLR prediction-context rooting
contracts above.
