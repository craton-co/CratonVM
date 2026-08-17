# G36-2 — `tlocal` is green under `--jdk-only`, and the two ITL rows that still are not

**Status:** MEASURED. `RJdkIntrinsics3 --only=tlocal` is `PASS (26 checks)` and
the whole vector is `PASS (1011 checks)` under `--jdk-only` on `9ae371468`,
with **no change from this lane**. Two oracle rows still diverge; both are
`phases_early.rs` and both are NOMINATED, not fixed — the lane was redirected
before they could be landed and neither can be verified without a build.

**Provenance.** Binary `C:/craton/target-rel3/release/cratonvm.exe` from
`9ae371468`, mtime `2026-08-17 07:07`. `target-rel2` (`9964ca733`) used as a
BEFORE. Oracle: Temurin 25.0.3+9-LTS. Probes written for this lane:
`G36Tl.java` (29 rows), `G36Cv.java` (11 rows), `G36Store.java`.

This record closes three open items in
`G5-1-inheritable-threadlocal-captures-at-construction-20260816.md` §8 and
corrects its §6 N1.

---

## 1. `tlocal` fell to `1eb5f8346`, and `G5-1` was wrong about why it could not

MEASURED, same vector, two binaries:

| binary | commit | `--only=tlocal` |
|---|---|---|
| `target-rel2` | `9964ca733` | **FAIL** — `AssertionError: tlocal:ITL value is copied at CONSTRUCTION, not read live: expected "parent-init", got "set-after-construction"` |
| `target-rel3` | `9ae371468` | **`PASS`, `tlocal=26, checks=26`** |

The difference is `1eb5f8346` (`G23-1`), which wired
`lang_system::capture_inheritable_tl_at_construction` into all nine
`Thread.<init>` natives in `lib.rs`.

`G5-1` §6 N2 said those nine "do **nothing** under `--jdk-only`, because there
these nine lose to the real forwarder bytecode — which is why N1 exists". That
is false, and the VM says so in its own output. From
`--dump-native-registry` + `--jdk-only-report` on `target-rel2`:

```text
java/lang/Thread.<init>()V   kind=bridge  owns_slot=true  invocations=2
                             overwrote=null  real_declaring_method.has_code=true
--jdk-only-report: {"class":"java/lang/Thread","method":"<init>","descriptor":"()V",
                    "native_kind":"bridge-ran-over-bytecode"}
```

The native runs and the real constructor does not — exactly `G34-1`'s rule,
that under `--jdk-only` a registered `Bridge` preempts real JDK bytecode at
`try_stackless_invoke` step 1 unless `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed.
So construction-time capture was live in `--jdk-only` the moment it was wired,
and no registration change was ever needed.

### 1.1 `G5-1` §6 N1 must NOT be taken, and its first option is incoherent

N1 offers two ways to stop the `ThreadLocal` Intrinsics owning the storage:
register them `Bridge`, or skip the registrar under `--jdk-only`.

* **The `Bridge` option does nothing it is meant to do.** `G34-1` §0: a `Bridge`
  preempts real bytecode too. Demoting would move six rows per class into the
  shadow census and change nothing about which body answers.
* **The skip-the-registrar option is a whole-process move of every ThreadLocal
  in the VM**, and it is not needed for the divergence `G5-1` existed to close,
  which `1eb5f8346` closed without it.

**Recorded so the next reader does not take it.** The standing note inside
`lang_system::native_thread_start0` still points at N1; correcting that comment
is N4 below.

## 2. MEASURED, first time on a binary — `G5-1` §4 is right

`G5-1` §8's first open item was whether the `--jdk-only` binary agrees that
nothing ever writes `Thread.inheritableThreadLocals`. `G36Store`, run with
`--add-opens java.base/java.lang=ALL-UNNAMED` on both VMs, after
`TL.set("x"); ITL.set("y")`:

| | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `Thread.threadLocals` | `java.lang.ThreadLocal$ThreadLocalMap` | **`null`** |
| `Thread.inheritableThreadLocals` | `java.lang.ThreadLocal$ThreadLocalMap` | **`null`** |
| `TL.get()` | `x` | `x` |
| `ThreadLocal$ThreadLocalMap` declaredMethods | 14 | **14** |
| `Thread.threadLocals` declared | `private ThreadLocalMap` | identical |

So the Intrinsic owns the storage, the master constructor's `ifnull` at pc 182
always skips, and `native_thread_start0`'s `createInheritedMap` block is dead —
all confirmed. The last two rows are new and cut the other way: the real
`ThreadLocalMap` class IS loadable with its full method table and
`Thread.threadLocals` IS a real, readable slot on the mirror, so N1's
skip-the-registrar option is *structurally* possible. It is still not
worthwhile (§1.1).

**Also corrected:** the assignment's premise that `CRATONVM_DISABLE_INTRINSICS=1`
approximates demoting the registration. It does not. `env_cache.rs:402` — it
only prevents a `CachedInvokeTarget::Intrinsic` inline-cache entry; ordinary
dispatch still returns `DispatchDecision::Intrinsic`. MEASURED: `--only=tlocal`
under `--nojit` with and without the flag fails identically on `target-rel2`.

## 3. The oracle table, and the two rows that still diverge

`G36Tl` (29 rows) on both VMs, `--jdk-only`. **27 of 29 identical.**

| # | case | HotSpot | CratonVM |
|---|---|---|---|
| 1–7 | `TL` bare / set / `set(null)` / remove / `withInitial` ×3 | `null,v1,null,null,supplied,over,supplied` | identical |
| 8 | ITL plain, `set` between ctor and `start` | `parent-init` | identical |
| 9 | ITL plain, constructed after the `set` | `set-after-construction` | identical |
| 10–11 | `new Thread(group, r, name)` — the executor shape | `group-name-init` / `group-name-after` | identical |
| 12 | anonymous `Thread` subclass, `set` after ctor | `sub-init` | identical |
| 13 | `Executors.newFixedThreadPool(1)` worker | `pool-after-poolcreate` | identical |
| 14 | thread constructed INSIDE a pooled worker | `worker-set` | identical |
| 15 | second task on the SAME worker | `worker-set-2` | identical |
| **16** | **`childValue` override** | **`cv(cvparent)`** | **`cvparent`** ← DIVERGES |
| 17 | parent unchanged by `childValue` | `cvparent-after` | identical |
| 18 | parent value explicitly `null` | `null` | identical |
| 19 | parent `remove()`d before ctor (empty capture must win) | `null` | identical |
| 20–21 | constructed, never started | `never-started-parent` / `false` | identical |
| 22 | `new Thread(g,r,n,0,false)` — `characteristics & 4` opt-out | `null` | identical |
| 23 | `new Thread(g,r,n,0,true)` opt-in | `optin-parent` | identical |
| **24** | **grandchild** | **`gp-init`** | **`null`** ← DIVERGES |
| 25–28 | child's writes are private; parent unaffected | `priv-parent,child-own,null,priv-parent` | identical |
| 29 | plain `ThreadLocal` does not cross | `null` | identical |

Rows 10/11 are worth stating plainly: **the executor shape behaves exactly like
the plain shape on both VMs.** The "non-null ThreadGroup AND name" mechanism
the original `native_thread_start0` note blamed is now falsified behaviourally
as well as by `javap` (`G5-1` §3).

### 3.1 Row 16 — `childValue` is never applied. `phases_early.rs`.

`snapshot_inheritable_tl_entries` copies each value verbatim; HotSpot applies
`InheritableThreadLocal.childValue(T)` inside `createInheritedMap`. The
function cannot call it: `tl_inheritable_ids()` is an `FxHashSet<i32>` of
identity hashes, so there is **no receiver to invoke it on**.

`G36Cv` pins the exact contract, all MEASURED on 25.0.3+9:

| # | question | HotSpot | CratonVM |
|---|---|---|---|
| 1 | child value | `cv(root)` | `root` |
| 2 | is it applied AGAIN at each generation? | **`cv(cv(root))`** | `root` |
| 3 | grandchild whose parent never read the ITL | `gp` | `null` |
| 4 | is it called for an explicitly stored `null`? | **`cv(null)`** | `null` |
| 5 | after the parent `remove()`d? | `null` (not called) | `null` |
| 6/8 | calls per constructed Thread — started / never started | **`1` / `1`** | `0` / `0` |
| 10 | which thread runs it? | **`main`** — the CONSTRUCTING thread | never runs |

So the fix is per-snapshot (not capture-once), must run for a stored `null`,
must not run after a `remove()`, must happen at construction even for a Thread
never started, and must run on the constructing thread — which is exactly where
`snapshot_inheritable_tl_entries` already is.

### 3.2 Row 24 — the drain is lazy, so inheritance is not transitive. `phases_early.rs`.

A child's inherited entries sit in `tl_inherited_pending` until its FIRST
`ThreadLocal` get/set/remove drains them. A thread that inherits a value and
then constructs a grandchild **without ever reading it** has an empty `TL_MAP`
when `snapshot_inheritable_tl_entries` runs, so the grandchild inherits
nothing. `G36Cv` row 3 isolates it: the same case where the child DOES read
first (`G36Tl` row 24 variant, `G36Cv` row 1→2) inherits correctly.

One line fixes it: `drain_inherited_for_current_thread(ctx)` as the first
statement of `snapshot_inheritable_tl_entries`. It is idempotent and a single
bool check after the first call.

## 4. NOMINATIONS

**N1 — `phases_early.rs`: apply `childValue`.** Change `tl_inheritable_ids()`
to carry the ThreadLocal OBJECT (`FxHashMap<i32, ObjectRef>`), rooted with
`register_var_handle_root` at `native_itl_init` and re-read with
`read_var_handle_root` at use — the pattern `tl_with_initial_suppliers`
already documents in the same file. Then in `snapshot_inheritable_tl_entries`
invoke `childValue` with the erased descriptor
`(Ljava/lang/Object;)Ljava/lang/Object;`, which reaches both the base
implementation and javac's bridge method for a `childValue(String)` override.

**The restructuring is not optional and is the risky part.** The current body
does all its work inside `tl_inheritable_ids().lock()` while holding
`TL_MAP.borrow()`. That is safe only because nothing in it calls into Java.
`childValue` is arbitrary application bytecode: one that reads any
`ThreadLocal` re-enters `TL_MAP.borrow()` — a `RefCell` double-borrow PANIC —
and one that constructs an `InheritableThreadLocal` re-enters
`native_itl_init` and the same non-reentrant `parking_lot::Mutex`, which is a
DEADLOCK on the thread-construction path, i.e. a hang in every executor. Three
passes: copy the table and drop the lock; read `TL_MAP` and drop the borrow;
call Java holding neither.

Fall back to the parent value on `Ok(None)` and on `Err(_)` — both equal the
base implementation. `Err` matters: synthetic-JDK images need not declare
`childValue`, and this function returns `Option`, not `Result`, while its
callers are nine `Thread.<init>` bodies returning `()`. An override that
THROWS would propagate on HotSpot and cannot here; that is a bounded deviation
and must be recorded at the site.

**N2 — `phases_early.rs`: drain before snapshotting.** §3.2. One line.

**N3 — `lang_system.rs`: gate `capture_inheritable_tl_at_construction` on
"does this process contain any `InheritableThreadLocal` at all".** Since
`1eb5f8346` every `Thread.<init>` in the VM — nine registrations, every pooled
worker `Executors.defaultThreadFactory()` mints, in every program — takes a
global lock and leaves a **permanent** `tl_inherited_pending` entry keyed by
the child's identity hash, plus a global GC root per captured value, whether or
not the program has ever heard of an `InheritableThreadLocal`. Entries are
removed only by the child's own first ThreadLocal access, so a worker that
never touches one, or a Thread constructed and never started (`G36Cv` row 8
measures HotSpot capturing there too), leaks for the life of the process. It
also enlarges the identity-hash collision surface from "threads with inherited
values" to "every thread".

The gate is sound because the set only grows: an empty set can only produce an
empty snapshot, and an absent queue entry then behaves identically, because
`native_thread_start0`'s fallback snapshot is `None` too. One known corner it
does not preserve: an ITL created AFTER a Thread is constructed but before it
is `start()`ed would fall back to start-time capture — which is what the VM did
before `1eb5f8346`, so it is a return to the status quo, not a new divergence.

**N4 — `lang_system.rs`: the note in `native_thread_start0` still points at
`G5-1` N1.** It should say instead that the divergence is closed by the nine
`lib.rs` bridges, that §2 above MEASURED the empty
`Thread.inheritableThreadLocals`, and §1.1's reason not to demote the
Intrinsics.

**N5 — `G5-1` §8's fourth open item is still open.** Whether
`native_sts_fork` runs at all under `--jdk-only` was not measured here;
`StructuredTaskScope` needs `--enable-preview` and belongs with the JEP 505
fixtures.

**N6 — a regression vector for rows 16 and 24.** `G5-1` §7's
`RJdkInheritableTL` covers neither `childValue` composition
(`cv(cv(root))`) nor the grandchild-whose-parent-never-read case. Both are in
`G36Cv`; the suite is another lane's tree.
