# G43-1 — `childValue`, transitivity, and a heap address published into a Java `int`

**Status:** FIXED IN SOURCE, NOT YET IN A BINARY. Two files changed,
`native-builtins/src/phases_early.rs` and
`native-builtins/src/jca/provider_chain.rs`. Every BEFORE number below is
MEASURED on `C:/craton/target-rel3/release/cratonvm.exe` (`9ae371468`, mtime
`2026-08-17 07:07`) against Temurin 25.0.3+9-LTS as oracle. **No AFTER number
is measured**, because this lane may not build — see §6, which says exactly
what is and is not evidenced.

Closes `G36-2` §4 N1 and N2. Falsifies `G30-1` §4.1's claim that
`pointer-into-primitive` never fires.

---

## 0. Provenance

| | |
|---|---|
| binary | `C:/craton/target-rel3/release/cratonvm.exe`, commit `9ae371468` |
| oracle | `java 25.0.3` Temurin-25.0.3+9-LTS |
| vectors | `C:/craton/cvm-mergecheck/regression-suite/build` (read-only) |
| probe | `scratchpad/g43/G43Itl.java`, written for this lane |
| instruments | `CRATONVM_DBG_COERCION=1`, `CRATONVM_DBG_OVERLAY=1`, `--dump-native-registry` |

---

## 1. The two ITL rows, MEASURED before

`G43Itl` on both VMs, CratonVM under `--jdk-only`. `null` is printed bare and
a value is printed in quotes, so an empty string and a null are distinguishable
on sight.

| row | case | HotSpot 25.0.3+9 | CratonVM `9ae371468` | |
|---|---|---|---|---|
| **16** | `childValue` override applied to the child's copy | `"cv(cvparent)"` | `"cvparent"` | **DIVERGES** |
| 17 | the parent's own value is untouched by it | `"cvparent"` | `"cvparent"` | agrees |
| **24** | grandchild, child never read the ITL first | `"gp-init"` | `null` | **DIVERGES** |
| 24ctl | same, but the child DOES read first | `"gp-init"` | `"gp-init"` | agrees |
| comp | is `childValue` applied AGAIN at each generation? | `"cv(cv(root))"` | `"root"` | **DIVERGES** |

Row 24ctl is the control that isolates row 24: the only difference between the
two is whether the intermediate thread performed one `ThreadLocal.get()`, and
that is enough to flip the answer. So row 24 is not "inheritance is broken", it
is "inheritance is not transitive **because the drain is lazy**".

The `comp` row is a consequence of 16 and 24 together and is the strongest
single statement of the contract: HotSpot applies `childValue` once per
generation, composing.

## 2. Row 16 — `childValue` was never applied, and why it could not be

`snapshot_inheritable_tl_entries` is CratonVM's `ThreadLocal.createInheritedMap`.
HotSpot's stores `key.childValue(e.value)`; ours copied `e.value` verbatim.

It could not do otherwise: `tl_inheritable_ids()` was an `FxHashSet<i32>` of
JLS identity hashes. A hash is enough to decide WHETHER an entry is inherited
and useless for deciding WHAT the child gets, because `childValue` is a virtual
method and **there was no receiver in the table to invoke it on**.

### 2.1 Fix

`tl_inheritable_ids()` is now `FxHashMap<i32, ObjectRef>` — identity hash → the
ThreadLocal object. `native_itl_init` registers the object with
`register_var_handle_root` and re-reads it with `read_var_handle_root` at use,
which is the pattern `tl_with_initial_suppliers` (same file, ~120 lines above)
already documents: the registry entry is what a moving GC remaps, and the raw
`ObjectRef` in the table is only a fallback for contexts that do not implement
the registry. The read-back key is the map key, because for this table the
object IS the thing the key hashes.

The invocation uses the ERASED descriptor
`(Ljava/lang/Object;)Ljava/lang/Object;`, which reaches both the base
`InheritableThreadLocal.childValue` and javac's synthetic bridge for a
`childValue(String)`-shaped override. `G43Itl.CvItl` is exactly that shape and
is the case row 16 measures.

### 2.2 Contract details, each MEASURED (`G36-2` §3.1's `G36Cv`, re-confirmed here)

* **Per snapshot, not capture-once** — hence `cv(cv(root))`.
* **Runs for an explicitly stored `null`.** A stored null IS an entry, so
  `childValue(null)` is called. A `remove()`d ThreadLocal is not an entry, so
  nothing is called. Both are unit-tested
  (`g43_1_child_value_runs_for_an_explicit_null_but_not_after_remove`).
* **Exactly once per constructed Thread, started or not.** This falls out of
  the existing structure and needed no change: the construction-time capture
  (`lang_system::capture_inheritable_tl_at_construction`, reached from the nine
  `Thread.<init>` bridges in `lib.rs`) and the start-time fallback in
  `native_thread_start0` are mutually exclusive —
  `inheritable_tl_captured_at_construction` gates the second on the first. Had
  they both run, `childValue` would have composed once per *start* as well as
  once per *construction* and a started thread would have seen `cv(cv(v))`.
* **Runs on the constructing thread**, which is where this function already was.

### 2.3 The bounded deviation, recorded at the site

HotSpot propagates an exception thrown by `childValue` out of `Thread.<init>`.
`snapshot_inheritable_tl_entries` returns `Option`, not `Result`, and its
callers are nine `Thread.<init>` bodies returning `()`. A throwing override is
therefore swallowed and the child inherits the parent value — which is what the
base implementation would have produced. `Ok(None)` (no body found; a
synthetic-JDK image need not declare `childValue`) takes the same arm.

This does NOT leak a pending exception: `MethodCallFailed::ExceptionThrown`
carries the `Throwable` in the `Err` value rather than in VM-global state, so
dropping the `Err` drops the exception with it.

## 3. Row 24 — the drain is lazy, so inheritance was not transitive

A child's inherited entries sit in `tl_inherited_pending` until its FIRST
`ThreadLocal` get/set/remove drains them into `TL_MAP`. A thread that inherited
a value and then constructed a child **without ever reading it** presented an
empty `TL_MAP` to the snapshot, so the grandchild inherited nothing.

Fix: `drain_inherited_for_current_thread(ctx)` is now the first statement of
`snapshot_inheritable_tl_entries`. It is idempotent and a single bool check
after the first call. The effect is to make "what this thread would see if it
read right now" the thing that gets snapshotted, which is what HotSpot's
`createInheritedMap` copies.

## 4. The hazard that stopped the previous lane, and how it was removed

`G36-2` §4 N1 named it and did not land the fix. The old body did all its work
inside `tl_inheritable_ids().lock()` **while holding `TL_MAP.borrow()`**. That
was safe only for as long as nothing in it called into Java. `childValue` is
arbitrary application bytecode, and two ordinary overrides break it:

* one that reads any `ThreadLocal` re-enters `TL_MAP.borrow_mut()` — a `RefCell`
  double-borrow **PANIC**;
* one that constructs an `InheritableThreadLocal` re-enters `native_itl_init`
  and the same non-reentrant `parking_lot::Mutex` — a **DEADLOCK on the
  thread-construction path**, i.e. a hang in every executor that ever mints a
  worker, which is every executor.

**The structure was changed first.** The body is now three passes and the split
is load-bearing, not cosmetic:

1. **Copy the table, drop the lock.** `tl_inheritable_ids().lock()` is taken
   inside a block that ends with a `Vec<(i32, ObjectRef)>`; the guard is
   released at the closing brace.
2. **Copy the values, drop the borrow.** `TL_MAP.borrow()` is held only across
   `Copy` reads of `ThreadLocalValue`. `ctx` is not touched inside the closure
   at all — the previous body called `tl_value_to_java`/`tl_value_from_java`
   under the borrow — so this pass cannot allocate, cannot re-enter, and cannot
   run a GC.
3. **Call Java holding neither.** Only here does `invoke_virtual` run.

Two consequences of pass 3 owning the Java calls:

* Values are converted with `tl_value_to_java` **immediately around** each call,
  not materialised in pass 2. `childValue` can allocate, and a pre-computed
  `ObjectRef` would be stale after a moving GC. The `ThreadLocalValue` carried
  between passes is the GC-stable form (its `Root` variant is a global-root
  handle). The `Ok(None)`/`Err(_)` fallback re-resolves the value *after* the
  call for the same reason.
* The ThreadLocal receiver is re-read through `read_var_handle_root` on every
  iteration, so a GC provoked by iteration *n* does not leave iteration *n+1*
  calling through a stale address.

The re-entrancy is pinned by a test that does BOTH hostile things from inside
the callback: `g43_1_child_value_may_reenter_the_threadlocal_machinery` installs
an `invoke_virtual` hook whose `childValue` constructs a fresh
`InheritableThreadLocal` (re-entering the mutex) and performs a real
`ThreadLocal.set` (re-entering the `RefCell`). Under the old structure that test
deadlocks or panics; that is the whole point of it.

## 5. `provider_chain.rs:317` — the coercion species that "never fires"

`G30-1` §4.1: *"No site in the census produces `pointer-into-primitive` and
none fired in the sweep."* MEASURED, `CRATONVM_DBG_COERCION=1`, `--jdk-only`,
`RCrypto`, **exactly two occurrences per run**, both this site:

```text
species="pointer-into-primitive" descriptor=I
  value=Object(Some(ObjectRef { ptr: 0x14a422576e8 }))   occurrence=0
species="pointer-into-primitive" descriptor=I
  value=Object(Some(ObjectRef { ptr: 0x14a4225d640 }))   occurrence=1

  9: cratonvm_vm::vm::vm_exec::impl$14::set_field
 10: cratonvm_native_builtins::jca::provider_chain::make_provider
              at native-builtins/src/jca/provider_chain.rs:317
 11: cratonvm_native_builtins::jca::provider_chain::resolve_service
```

`gc/src/heap.rs:2068`'s `b'I'` arm turns that into
`Value::Int(o.as_ptr() as usize as i32)` — **a heap address, truncated to 32
bits, published into a Java `int`**.

### 5.1 The comment justifying the write was false, and so was the layout it cited

Line 313–314 said the write existed to keep `phases_early::register_phase53_security`'s
readers consistent. Two things are wrong with that.

**First, that registrar does not exist under `--jdk-only`.** MEASURED, not
inferred: `--dump-native-registry` over a 10,691-row `--jdk-only` RCrypto run
has **13** `java/security/Provider` rows and **4** `java/security/Provider$Service`
rows, and the `registered_by` of every single one is `src/jca/provider_chain.rs`.
Zero rows from `phases_early.rs`. (Source side agrees: `register_phase53_security`
has exactly one non-test caller, `register_phase53_natives`, itself reached only
from `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`.)

**Second, `register_phase53_security`'s `getInfo` Bridge does not read slot 2
even in synthetic mode** — it synthesises its string from the *name* at slot 0.
The only reader of Provider slot 2 anywhere in the tree is `provider_get_info`
in `provider_chain.rs` itself.

**Third — and this is the part that made the write look harmless — the
real-JDK layout the accessors' comment described was fiction.** It claimed
slots 0+1 were `serialVersionUID` (long), slot 2 `debug`, slot 3 `name`, …
`javap -p java.security.Provider` on 25.0.3+9: `serialVersionUID` and `debug`
are **both `static`** and occupy no instance slot at all. And
`java.security.Provider extends java.util.Properties extends java.util.Hashtable
extends java.util.Dictionary`, so the low slots belong to the SUPERCLASSES.
Corroborated by the VM's own instrument — `CRATONVM_DBG_OVERLAY=1` on the same
run prints, for the loaded image:

```text
[OVERLAY-LAYOUT] java/util/Hashtable — model has 16 slot(s), 4 disagree with the loaded image
[OVERLAY-LAYOUT]   slot  0 ok   model=_f0:Ljava/lang/Object; real=table:[Ljava/util/Hashtable$Entry;
[OVERLAY-LAYOUT]   slot  1 TYPE model=_f1:Ljava/lang/Object; real=count:I
[OVERLAY-LAYOUT]   slot  2 TYPE model=_f2:Ljava/lang/Object; real=threshold:I
```

So the legacy mirror wrote a `String` over the hash table, a `Double` over
`count`, and a heap address over `threshold` — and the coercion instrument's
`descriptor=I` at slot 2 independently confirms slot 2 is an `int` on a real
`Provider`.

### 5.2 Why it has not blown up yet, and why that is luck

`make_provider` deliberately sets `initialized = 1` (see its own comment) so
that real `Provider.keys()` / `entrySet()` / `Security.getAlgorithms` bytecode
RUNS over these objects rather than throwing `IllegalStateException`. That
bytecode is `Hashtable`'s, over the fields we just corrupted.

It survives because `Hashtable.getEnumeration` early-returns on `count == 0`,
and `count` happens to be zero: the `b'I'` arm stores a `Double` as
`d.to_bits() as i32`, and the low 32 bits of `25.0` (`0x4039_0000_0000_0000`)
are exactly `0`. Any version with a fractional part is not zero — `1.8` is
`0x3FFC_CCCC_CCCC_CCCD`, low half `0xCCCCCCCD`, i.e. `count == -858993459` —
and the next `keys()` constructs an `Enumerator` and walks the `String` in
`table` as an `Entry[]`.

### 5.3 The decision: the mirror is synthetic-mode-only, per instance

The two modes need different answers and the record says so explicitly rather
than picking one.

* **Real-JDK mode.** There is nothing to "correct the slot indices to". Slots
  0/1/2 are somebody else's fields, and Provider's own `name` / `version` /
  `versionStr` / `info` already receive four `set_field_by_name` writes eight
  lines above. Relocating the mirror onto those slots would merely repeat those
  writes; pointing it anywhere else corrupts a live `Hashtable`. **The mirror
  must not run.**
* **Synthetic-jdk mode.** There is no `Hashtable` and no named field to resolve;
  slots 0/1/2 ARE name/version/info and `provider_get_name` /
  `provider_get_version` / `provider_get_info` read them. **The mirror must
  run.** This is the trap the previous lane named — a "fix" that corrects
  indices to the real layout breaks the mode where the code actually runs and
  changes nothing under `--jdk-only`.

So the mirror is gated on a per-instance read-back predicate,
`provider_has_named_layout`, which is the same shape as the
`service_has_named_layout` this file already uses for `Provider$Service`: write
by name, then ask whether the write took. It answers correctly in both modes,
needs no new `NativeContext` surface, and is per-instance where a registration
would be global.

**Witness field: `versionStr`, not `name`.** Substantively, `versionStr` is
declared by `java.security.Provider` itself and by nothing above it, so
satisfying it proves Provider's own layout rather than a superclass's.
Mechanically, the unit-test mock resolves the bare name `name` through a
class-blind mirror table (`test_utils::mock_jdk_field_slot`, which maps
`"name" → 1` for any class), so a `name`-based predicate would answer "real
layout" under every test in the file and leave the synthetic arm untestable —
the DIVERGENCE that `MockNativeContext::get_field_by_name`'s own doc comment
warns has already cost this campaign twice. Choosing a witness the mock does
not fabricate is the difference between a test and a tautology.

### 5.4 Two readers hardened alongside it

`provider_get_name` and `provider_get_info` both declare `()Ljava/lang/String;`
and both fell back to a raw slot read. On a real `Provider` that slot is
`Hashtable.table` and `Hashtable.threshold:I` respectively, and an unwritten
reference slot reads back as `Int(0)` besides. Both now return
`Value::Object(None)` rather than handing the interpreter an `Int` from a
reference-returning native.

## 6. What is evidenced and what is not

**Evidenced.** Everything in §1 (both VMs, this binary). The two
`pointer-into-primitive` occurrences and their backtrace (§5). The registry
census (§5.1). The `Hashtable` layout (§5.1). The nine regression vectors below,
green on the unmodified binary, i.e. the baseline these changes must not move.

**NOT evidenced.** The AFTER. This lane may not run `cargo build`. The
"fires twice, then not at all" proof the assignment asks for is therefore
**half-measured**: the BEFORE is on the record above with a backtrace naming the
line; the AFTER is a source-level argument plus three unit tests that pin the
predicate's two arms and the reader's degradation. Rows 16 and 24 are likewise
BEFORE-only. `rustfmt --edition 2021 --check` parses both files and produces
**no hunk that was not already present** in the pre-change file (both files were
already not fully rustfmt-clean; the baseline was captured from
`git show HEAD:` and diffed hunk-body against the post-change output).

**The single highest-value follow-up is to build and re-run**
`CRATONVM_DBG_COERCION=1 cratonvm --jdk-only RCrypto` and confirm the
`pointer-into-primitive` count goes 2 → 0, and `G43Itl` and confirm rows 16/24
go `cvparent`/`null` → `cv(cvparent)`/`gp-init`.

## 7. Baseline vectors, MEASURED on `9ae371468` (unmodified binary)

| vector | result |
|---|---|
| `RJdkIntrinsics3` | `PASS (1011 checks)` |
| `RJdkIntrinsics3 --only=tlocal` | `PASS (26 checks)`, `tlocal=26` |
| `RJdkExecutors` | `PASS (69 checks)` |
| `RCrypto` | `PASS (57 checks)` |
| `RJdkForkJoin` | `PASS (42 checks)` |
| `RExecutorShutdown` | `PASS` |
| `RJdkAqs` | `PASS (60 checks)` |
| `RJdkPhaser` | `PASS (240 checks)` |
| `RJdkHello` | `PASS (41 checks)` |
| `RJdkSecurity` | `PASS (153 checks)` |

`RJdkExecutors` is at 69 and must stay there; the executor path is exactly what
the deadlock in §4 would have hung, which is why §4 is a structural change and
not an added `try_lock`.

## 8. Settled, and not to be undone

`tlocal` is green (`G36-2` §1) and the `ThreadLocal` /
`InheritableThreadLocal` registrations are unchanged. `G5-1` §6 N1 stays
un-taken: `G36-2` §1.1 measured it incoherent, because a `Bridge` preempts real
JDK bytecode too (`G34-1` §0), so demoting the Intrinsics moves rows between
censuses and changes nothing about which body answers.

## 9. NOMINATIONS (outside this lane's two files)

**N1 — `lang_system.rs`: `capture_inheritable_tl_at_construction` should be
gated on "does this process contain any `InheritableThreadLocal` at all".**
This is `G36-2` N3, restated because this change makes it slightly worse and
much easier. Since `1eb5f8346` every `Thread.<init>` in the VM — nine
registrations, every pooled worker `Executors.defaultThreadFactory()` mints, in
every program — takes a global lock and leaves a permanent `tl_inherited_pending`
entry keyed by the child's identity hash, whether or not the program has ever
heard of an `InheritableThreadLocal`. Entries are removed only by the child's
own first ThreadLocal access, so a worker that never touches one leaks for the
life of the process. The gate is sound because the set only grows and an empty
set can only produce an empty snapshot; the guard is now a one-liner because
`tl_inheritable_ids()` is a map with an `is_empty()`.

Note the interaction this lane introduces: `snapshot_inheritable_tl_entries`
now DRAINS before it checks, so the early `return None` for an empty table
happens after the drain, not before. That ordering is required (a thread's
pending entries must land in `TL_MAP` before anything decides the map is empty)
and it means the per-`Thread.<init>` cost is now one thread-local bool check
plus, on the first construction only, one map probe. N1 removes the rest.

**N2 — `lang_system.rs`: the standing note in `native_thread_start0` still
points at `G5-1` N1**, which `G36-2` §1.1 measured incoherent. It should say
instead that the divergence is closed by the nine `lib.rs` bridges, and that
`G36-2` §2 MEASURED `Thread.inheritableThreadLocals` empty on both this VM and
HotSpot's reflection view. (`G36-2` N4, still open.)

**N3 — the regression suite has no vector for rows 16 or 24.** `G5-1` §7's
`RJdkInheritableTL` covers neither `childValue` composition (`cv(cv(root))`) nor
the grandchild-whose-parent-never-read case. `scratchpad/g43/G43Itl.java` is a
ready-made five-row fixture for both; the suite is another lane's tree.

**N4 — `gc/src/heap.rs`: `pointer-into-primitive` should carry provenance.**
Both occurrences report `class_id=-1 index=-1 access="unattributed"`, so the
only way to identify the site is `CRATONVM_DBG_COERCION=1`'s backtrace. That
worked here, but it means the species is invisible in an ordinary run —
which is a large part of why `G30-1` §4.1 could conclude it never fires. (This
is `G30` NOMINATION 1; this lane is a worked example of its cost.)

**N5 — `gc/src/heap.rs`: the `b'I'` arm stores a `Double` as
`d.to_bits() as i32`, not `d as i32`.** §5.2 shows this is what kept the
Provider corruption latent (`25.0` → `0`) and what would have exposed it for any
fractional version. Whether bit-reinterpretation or numeric conversion is the
right rule for a descriptor-driven coercion is a question for the owner of that
file; the point for the record is that the current rule makes the blast radius
of a mis-slotted `Double` depend on its VALUE, which is the worst property a
failure mode can have.

**N6 — a second `make_provider` exists.** `register_phase53_security` declares
its own nested `make_provider` writing slots 0/1 only. It is synthetic-only and
consistent with its own readers, so it is not a defect — but it is a second
implementation of the same object, and the two have already drifted once (this
one never learned about `info`, `versionStr`, or `initialized`). Worth folding
into the real one when someone owns both files.

## 10. What this lane could not settle

* **Whether the fix works.** See §6.
* **Whether any caller passes `make_provider` a fractional version**, which is
  the difference between §5.2's latent corruption and a live one. The seed
  chain in this file is all whole numbers, but `Security.addProvider` of an
  application provider reaches `make_provider` through
  `resolve_or_make_provider` with whatever version that provider reports.
* **`G5-1` §8's fourth open item** — whether `native_sts_fork` runs at all
  under `--jdk-only`. `StructuredTaskScope` needs `--enable-preview` and belongs
  with the JEP 505 fixtures. (`G36-2` N5, still open.)
