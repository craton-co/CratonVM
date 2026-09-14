# G5-1 — `InheritableThreadLocal` captures at construction, and the note that named the wrong cause

**Status:** PARTIALLY FIXED. The `--jdk-only` half is a NOMINATION, not a fix.
**Provenance:** every "HotSpot" row below is **MEASURED** on Temurin
25.0.3+9-LTS (`$JAVA_HOME`), 2026-08-16. Every claim about CratonVM's code is
**SOURCE-VERIFIED** by reading the registrar and the dispatch resolver.

> **NOTHING IN THIS RECORD IS MEASURED ON A CRATONVM BINARY.** This lane could
> not build or run the VM. The single CratonVM row in §1 is quoted from F41-1,
> which did measure it. Every statement about what CratonVM does *after* this
> change is **PREDICTED**. Read §8 before believing any of it.

Probes: `ItlProbe.java`, `StsItl.java` (oracle only; sources reproduced in §7
and §9 so they can be re-run without this scratch directory).
Oracle disassembly: `javap -p -c java.lang.Thread`.

---

## 0. The headline

Three separate things were wrong, and only the first was known:

| # | | evidence |
|---|---|---|
| 1 | ITL is captured at `start()`, not at construction | MEASURED (F41-1 §6) |
| 2 | the site's written root cause — "the ThreadGroup+name constructor drops the copy" — **cannot be true**; the bytecode has no such branch | SOURCE-VERIFIED (§3) |
| 3 | `StructuredTaskScope.fork` inherits **nothing at all**, because it bypasses `native_thread_start0` entirely | SOURCE-VERIFIED (§4); HotSpot inherits, MEASURED (§2 row 14) |

Defect 3 is fixed in this lane. Defect 2 is corrected in place. Defect 1 is
**gated but not closed under `--jdk-only`** — see §5 and §6.

---

## 1. The defect, restated

MEASURED, F41-1 §6:

```java
ITL.set("parent-init");
Thread t = new Thread(() -> seen[0] = ITL.get());
ITL.set("set-after-construction");
t.start();
```

| | HotSpot | CratonVM |
|---|---|---|
| child sees | `parent-init` | `set-after-construction` |

---

## 2. The oracle: what the contract actually is

**MEASURED**, `ItlProbe` + `StsItl` on Temurin 25.0.3+9. All labels ASCII
(HANDOFF §7 — a differential once failed on a single em-dash).

| # | case | HotSpot |
|---|---|---|
| 1 | `new Thread(r)`, `set` between ctor and `start` | `parent-init` |
| 2 | same, thread constructed *after* the `set` | `set-after-construction` |
| 3 | `new Thread(group, r, name)` (the executor shape), `set` between ctor and `start` | `group-name-init` |
| 4 | same, constructed after the `set` | `group-name-after` |
| 5 | `Executors.newFixedThreadPool(1)` worker | `pool-after-poolcreate` |
| 6 | thread constructed *inside* a pooled worker (nested inheritance) | `worker-set` |
| 7 | `childValue` override | `cv(cvparent)` |
| 8 | parent's value explicitly `null` | `null` |
| 9 | parent `remove()`d before constructing, `set` again after | `null` |
| 10 | constructed, never started | parent still reads its own value; `alive=false` |
| 11 | `new Thread(g, r, n, 0, false)` (opt out) | `null` |
| 12 | `new Thread(g, r, n, 0, true)`, `set` after ctor | `optin-parent` |
| 13 | grandchild (child constructs a grandchild) | `gp-init` |
| 14 | `StructuredTaskScope.fork` subtask | `scope-parent` |
| 15 | `Thread.ofVirtual().unstarted(r)`, `set` after ctor | `vt-parent` |

Rows 3 and 4 are the pair that matters most: the executor shape behaves
**exactly like the plain shape**. Row 5 is not a counter-example — a
`newFixedThreadPool` worker is constructed lazily on first `submit`, so its
construction moment is genuinely after the second `set`. Row 9 is the row a
naive fix fails: an *empty* capture must still suppress a later re-capture.
Row 15 says the rule is about `Thread` construction, not about platform
threads.

---

## 3. The oracle's constructor chain — and why the site's root cause is impossible

**SOURCE-VERIFIED**, `javap -p -c java.lang.Thread` on 25.0.3+9.

`java.lang.Thread` declares eleven constructors. Eight are public and every
one of them is a three-to-five instruction forwarder ending in the **same**
`invokespecial`:

```
Thread()                              -> <init>(ThreadGroup,String,I,Runnable,J)
Thread(Runnable)                      -> same
Thread(ThreadGroup,Runnable)          -> same
Thread(String)                        -> same   (via checkName)
Thread(ThreadGroup,String)            -> same   (via checkName)
Thread(Runnable,String)               -> same   (via checkName)
Thread(ThreadGroup,Runnable,String)   -> same   (via checkName)
Thread(ThreadGroup,Runnable,String,J) -> same   (via checkName)
Thread(ThreadGroup,Runnable,String,J,Z) -> same (via checkName;
                                          characteristics = inherit ? 0 : 4)
```

The two non-public ones are the master
`Thread(ThreadGroup, String, int characteristics, Runnable, long)` and the
VM-internal `Thread(String, int, boolean)`.

**The inheritance copy exists in exactly one place**, pc 164..204 of the
master constructor:

```
 164: iload  8          // attaching == (currentThread() == this)
 166: ifne   235        //   attaching -> skip inheritance entirely
 169: iload_3           // characteristics
 170: iconst_4
 171: iand
 172: ifne   222        //   NO_INHERIT_THREAD_LOCALS -> skip
 175: aload  7          // parent (== currentThread())
 177: getfield  #16     //   parent.inheritableThreadLocals
 182: ifnull 204        //   null -> skip
 189: ThreadLocalMap.size()
 192: ifle   204        //   empty -> skip
 198: invokestatic ThreadLocal.createInheritedMap(...)
 201: putfield  #16     //   this.inheritableThreadLocals = ...
```

Note pc 201 — the site's own note names "offset 201 in the real master
constructor", so its author read this same disassembly.

**Between pc 164 and pc 204 the bytecode reads three things:** `attaching`,
`characteristics`, and `parent.inheritableThreadLocals`. It does **not** read
local 1 (`group`) or local 2 (`name`). The `putfield` at 201 is
unconditional given the four guards above it, and none of them mentions the
group or the name.

Therefore the note in
`native-builtins/src/lang_system.rs::native_thread_start0` —

> "silently doesn't take effect for a still-unexplained interpreter reason
> **specifically when BOTH the ThreadGroup and name constructor arguments are
> explicitly non-null at the same time** … root-causing it further needs
> interpreter-level bytecode tracing"

— **states a mechanism that the class file makes impossible.** There is no
overload-specific copy to lose, no field-slot difference between overloads
(one `putfield #16`), and no delegation gap (all eight public forms reach the
same `invokespecial`). Whatever the "minimal repro" observed, the cause was
not the constructor branching on group and name.

**Evidence class: SOURCE-VERIFIED (oracle bytecode).** It is not a measurement
on CratonVM, and it does not need to be: it is a statement about what the JDK
class file can do at all.

---

## 4. The real root cause: ThreadLocal never touches `Thread.inheritableThreadLocals`

**SOURCE-VERIFIED**, three files:

1. `native-builtins/src/phases_early.rs:3681`
   `register_thread_local_natives` opens with
   `r.set_category(cratonvm_native_api::NativeKind::Intrinsic)` and registers
   `<init>`/`initialValue`/`get`/`set`/`remove`/`withInitial` on **both**
   `java/lang/ThreadLocal` and `java/lang/InheritableThreadLocal`. Their store
   is `TL_MAP`, a Rust `thread_local!` keyed by the ThreadLocal's identity
   hash (same file, ~3563).
2. `vm/src/vm/vm_exec.rs:849`, inside `resolve_native_dispatch_wave1`:
   `NativeKind::Intrinsic => Some(DispatchDecision::Intrinsic(callback))` —
   reached **after** the `policy.is_jdk_only()` branch at 830. §1.4's reviewed
   exception. An `Intrinsic` beats concrete real-JDK bytecode under
   `--jdk-only` too.
3. It is registered in real-JDK mode: `register_thread_local_natives` is
   called from `lib.rs:9247` (inside `register_essential_natives_with_shims`,
   `lib.rs:7190`) and from `phases_early.rs:3380`
   (`register_phase50_natives`). Neither is synthetic-only.

**Consequence.** Nothing in the process ever writes `Thread.threadLocals` or
`Thread.inheritableThreadLocals`. So in the master constructor, `ifnull 204`
at pc 182 **always** takes the skip branch — for every overload, group and
name irrelevant. The constructor copy is inert not for one shape but for all
of them.

**This is the HANDOFF §5 trap, sitting inside the very site this record is
about.** `native_thread_start0`'s `createInheritedMap` block asks the same
question from the native side:

```rust
if let Value::Object(Some(parent_map)) =
    ctx.get_field_by_name(parent, "inheritableThreadLocals")
```

That binding **can never succeed**, for the same reason. It is a fix that
lives in a body with, effectively, `invocations = 0`. The behaviour F41-1
measured is produced entirely by the *other* block, the `TL_MAP` snapshot at
`lang_system.rs:1295` — `snapshot_inheritable_tl_entries` +
`queue_inherited_tl_for_child`.

**How I established which body runs, given I cannot run
`--dump-native-registry`:** by the registrar's stated category
(`set_category(Intrinsic)`, phases_early.rs:3683) plus the resolver's
`Intrinsic` arm (vm_exec.rs:849) being on the far side of the
`is_jdk_only()` test. That is a source chain with no comment in it — the
handoff's warning is that *comments* lie, and this chain is a `set_category`
call and a `match` arm. **It is still SOURCE-VERIFIED, not measured**, and
§8 says what would settle it.

Three real-JDK behaviours die with the field, all MEASURED as present on the
oracle and all inexpressible in `TL_MAP`:

* construction-time capture (§2 rows 1, 3, 9, 12, 15);
* the `characteristics & 4` opt-out (§2 row 11) — the site note already
  admits `characteristics` "isn't retained anywhere observable";
* `InheritableThreadLocal.childValue(T)` (§2 row 7), which the JDK applies
  inside `createInheritedMap`.

---

## 5. What changed in this lane

### 5.1 `native-builtins/src/lang_system.rs`

**New `capture_inheritable_tl_at_construction(ctx, child)`** (next to
`thread_already_started`, which exists for the same anti-twin reason). It
snapshots the constructing thread's inheritable entries and queues them
against `child`'s identity hash — **including an empty snapshot**, which §2
row 9 requires.

**New `inheritable_tl_captured_at_construction(child_hash)`.** It asks
`phases_early::tl_inherited_pending()` whether an entry exists, rather than
keeping a second side flag. Deliberate: the predicate and the data it guards
are then the same object and cannot drift.

**`native_thread_start0` now defers to a construction-time capture.** Both of
its ITL blocks — the `TL_MAP` snapshot and the (dead) `createInheritedMap`
field copy — are skipped when the child was already captured at construction.

**Why this cannot break the executor paths, concretely.** With no construction
capture recorded for a child, `inheritable_tl_captured_at_construction`
returns `false` and both blocks run exactly as before, in the same order, on
the same values. `tl_inherited_pending()` is the map that already existed and
was already consulted only by the child's drain; a `contains_key` on it adds
one uncontended `parking_lot` lock acquisition per `start0`. **No
`Executors`-created thread has a construction-time capture today** (§6 N1/N3
are what would give it one), so every pooled worker takes the identical path
it took before this change. That is also the honest weakness: see §8.

**The note is corrected in place.** The original text is retained verbatim,
labelled as the original, and followed by the §3 disassembly and the §4
dispatch chain.

### 5.2 `native-builtins/src/jdk25_concurrency.rs`

`native_sts_fork` builds its subtask worker `Thread` and calls
`ctx.thread_start(worker)` **directly** (line ~934), bypassing
`native_thread_start0`. So a forked subtask inherited **no** ITL values at
all. HotSpot inherits — MEASURED, §2 row 14 (`StructuredTaskScope.fork` builds
its thread through a `Thread.Builder` whose `inheritInheritableThreadLocals`
defaults to true).

The capture call is placed after **both** layout arms (synthetic and
real-JDK), because the real-JDK arm's `ctx.invoke("java/lang/Thread",
"<init>", ...)` can allocate and the queue key must be the finished object's
identity, and before `register_scope_fork` / `thread_start`.

For this site construction and start are adjacent, so the *timing* half is not
observable here; the *inheritance* half is a behaviour change from "nothing"
to "the fork-time values".

### 5.3 `vm/src/runtime/interpreter/native_override.rs`

Documentation only. `redefine_immune_thread_local_native`'s comment already
explained that ThreadLocal's values live in `TL_MAP`, but read as a
`Compatible`-mode story. It now states the `--jdk-only` consequence (§4) and
names the three JDK behaviours that fall with it, so the next reader asking
"which body runs" lands on the answer instead of on the disproved note.

**No dispatch behaviour changed in this file.** Not one predicate was edited.

---

## 6. NOMINATIONS

Everything that would actually close the `--jdk-only` timing divergence is
outside this lane's three files.

### N1 — the real fix. `native-builtins/src/phases_early.rs:3681`, `register_thread_local_natives`

Line 3683 is `r.set_category(cratonvm_native_api::NativeKind::Intrinsic);`.
Under `--jdk-only` these six-per-class registrations must not shadow real
bytecode: either register with `NativeKind::Bridge` (§7 step 3 then sends
`--jdk-only` to the real body while `Compatible` is bit-for-bit unchanged) or
skip the registrar entirely when the policy is `JdkOnly`.

Real `ThreadLocal` bytecode then stores into `Thread.threadLocals` /
`Thread.inheritableThreadLocals`, and the master constructor's pc 175..201
does the copy at construction — which buys **all fifteen rows of §2 at once**,
including `childValue` and the `characteristics & 4` opt-out that no
`TL_MAP`-side fix can express.

**This composes correctly with §5.1 with no further edit**: with the natives
demoted, `tl_inheritable_ids()` is never populated, so
`snapshot_inheritable_tl_entries` returns `None` and the snapshot block is
inert; and the constructor writes `inheritableThreadLocals`, so
`child_itl_unset` is false and the field-copy block is skipped. `start0`
becomes fully inert on its own.

**Blast radius, stated honestly:** every `ThreadLocal` in the process moves
storage. It requires `Thread.currentThread()` to return a real-layout mirror
with a working `threadLocals` slot, and `ThreadLocal$ThreadLocalMap` bytecode
to run. Neither is verified here. This is the largest of the nominations and
the one that needs a build.

### N2 — `native-builtins/src/lib.rs`, `register_essential_natives_with_shims` (7190)

Nine `Thread.<init>` natives are registered at lines **13705, 13721, 13742,
13770, 13794, 13817, 13841, 13876, 13908** (descriptors `()V`,
`(Runnable)V`, `(String)V`, `(Runnable,String)V`, `(ThreadGroup,Runnable)V`,
`(ThreadGroup,String)V`, `(ThreadGroup,Runnable,String)V`,
`(...String;J)V`, `(...String;JZ)V`).

Add as the first statement of each body, after `this` is bound:

```rust
crate::lang_system::capture_inheritable_tl_at_construction(ctx, this);
```

Nine lines. It closes §2 rows 1/3/9/12 in `Compatible` and synthetic-JDK mode.
It is safe by construction: it only ever queues the entry `start0` would have
queued, earlier; `start0` then declines to re-snapshot (§5.1).

**It does nothing under `--jdk-only`**, because there these nine lose to the
real forwarder bytecode — which is why N1 exists.

### N3 — the missing master-constructor descriptor

There is **no** registration anywhere for
`("java/lang/Thread", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/String;ILjava/lang/Runnable;J)V")`
— the one constructor all eight public forms funnel into. If N1 is judged too
large, the alternative is a wrapper registered on that descriptor which calls
`capture_inheritable_tl_at_construction` and then
`ctx.invoke_special_bytecode_only` for the real body, plus the triple added to
`force_native_over_real_jdk_bytecode`.

**Recorded, not recommended.** It has to be `Intrinsic` to win under
`--jdk-only`, which is a §1.4 policy statement; and a wrapper that re-enters
its own method would break *every* thread in the VM, executors first. It must
not be attempted by a lane that cannot run the binary. N1 is strictly safer.

### N4 — five more `ctx.thread_start` sites that bypass `native_thread_start0`

Same defect as §5.2, still open. Each needs
`crate::lang_system::capture_inheritable_tl_at_construction(ctx, <thread>)`
immediately before its `thread_start`:

* `native-builtins/src/lib.rs:22850`, `:22888`, `:22905`
* `native-builtins/src/net_phase_e.rs:17344` (the HTTP dispatcher worker)
* `native-builtins/src/phases_late/concurrent.rs:4168`, `:4231`

Whether HotSpot inherits at each of these depends on what each spawn models;
that was not measured here. Do not apply blindly (HANDOFF §5: "do not
generalise a contract from three rows").

### N5 — observation, unsettled

`register_essential_natives_with_shims` (lib.rs:7190) sets no category of its
own, and the last `set_category` before line 13705 is the `regex_category`
restore at 7786. The nine `Thread.<init>` registrations therefore run on the
**ambient** category inherited from the caller — the condition
`no_registration_runs_on_the_ambient_default` exists to end. Their effective
kind was not settled by this lane. `--dump-native-registry`'s `kind` column
settles it in one run.

---

## 7. The regression vector I would add

`regression-suite/` is another lane's tree, so this is source only — do not
create the file from this record. All printed labels ASCII (HANDOFF §7).

```java
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

public class RJdkInheritableTL {
    static int checks = 0, failed = 0;
    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>();
    static final InheritableThreadLocal<String> CV = new InheritableThreadLocal<>() {
        @Override protected String childValue(String p) {
            return p == null ? "cv(null)" : "cv(" + p + ")";
        }
    };

    static void check(String label, Object actual, Object expected) {
        checks++;
        boolean ok = String.valueOf(actual).equals(String.valueOf(expected));
        if (!ok) failed++;
        System.out.println((ok ? "ok   " : "FAIL ") + label
                + " = " + actual + " (expected " + expected + ")");
    }

    static String childSees(Thread t, AtomicReference<String> box) throws Exception {
        t.start();
        t.join();
        return box.get();
    }

    public static void main(String[] args) throws Exception {
        ThreadGroup g = Thread.currentThread().getThreadGroup();
        AtomicReference<String> box = new AtomicReference<>("UNSET");
        Runnable r = () -> box.set(String.valueOf(ITL.get()));

        // 1. plain Thread: the capture is at construction, not at start
        ITL.set("parent-init");
        Thread t1 = new Thread(r);
        ITL.set("set-after-construction");
        check("plainThread ctorThenSet", childSees(t1, box), "parent-init");

        // 2. a thread constructed after the set sees the new value
        check("plainThread constructedAfterSet",
              childSees(new Thread(r), box), "set-after-construction");

        // 3. the executor shape behaves identically to the plain shape
        ITL.set("group-name-init");
        Thread t3 = new Thread(g, r, "rjdkitl-3");
        ITL.set("group-name-after");
        check("groupRunnableName ctorThenSet", childSees(t3, box), "group-name-init");

        // 4. same shape, constructed after the set
        check("groupRunnableName afterSet",
              childSees(new Thread(g, r, "rjdkitl-4"), box), "group-name-after");

        // 5. a pooled worker is constructed lazily, at first submit
        ITL.set("pool-init");
        ExecutorService pool = Executors.newFixedThreadPool(1);
        ITL.set("pool-after-poolcreate");
        check("pooledWorker",
              pool.submit(() -> String.valueOf(ITL.get())).get(), "pool-after-poolcreate");

        // 6. nested inheritance: a thread constructed inside a pooled worker
        check("nestedInsidePooledWorker", pool.submit(() -> {
            ITL.set("worker-set");
            AtomicReference<String> inner = new AtomicReference<>("UNSET");
            Thread n = new Thread(() -> inner.set(String.valueOf(ITL.get())));
            ITL.set("worker-set-2");
            n.start(); n.join();
            return inner.get();
        }).get(), "worker-set");
        pool.shutdownNow();

        // 7. childValue is applied, to the construction-time parent value
        AtomicReference<String> cvBox = new AtomicReference<>("UNSET");
        CV.set("cvparent");
        Thread t7 = new Thread(() -> cvBox.set(String.valueOf(CV.get())));
        CV.set("cvparent-after");
        check("childValueOverride", childSees(t7, cvBox), "cv(cvparent)");

        // 8. a null parent value is inherited as null
        ITL.set(null);
        Thread t8 = new Thread(r);
        ITL.set("non-null-after-null");
        check("parentValueNull", childSees(t8, box), "null");

        // 9. remove() before construction: an EMPTY capture must still win
        ITL.set("before-remove");
        ITL.remove();
        Thread t9 = new Thread(r);
        ITL.set("after-remove");
        check("parentRemovedBeforeCtor", childSees(t9, box), "null");

        // 10. constructed but never started
        ITL.set("never-started-parent");
        Thread t10 = new Thread(() -> { });
        check("neverStarted parentUnaffected", ITL.get(), "never-started-parent");
        check("neverStarted notAlive", t10.isAlive(), false);

        // 11. explicit opt-out
        AtomicReference<String> ooBox = new AtomicReference<>("UNSET");
        ITL.set("optout-parent");
        Thread t11 = new Thread(g, () -> ooBox.set(String.valueOf(ITL.get())),
                                "rjdkitl-11", 0, false);
        check("inheritThreadLocalsFalse", childSees(t11, ooBox), "null");

        // 12. explicit opt-in still captures at construction
        AtomicReference<String> oiBox = new AtomicReference<>("UNSET");
        ITL.set("optin-parent");
        Thread t12 = new Thread(g, () -> oiBox.set(String.valueOf(ITL.get())),
                                "rjdkitl-12", 0, true);
        ITL.set("optin-after");
        check("inheritThreadLocalsTrue", childSees(t12, oiBox), "optin-parent");

        // 13. grandchild
        AtomicReference<String> gcBox = new AtomicReference<>("UNSET");
        ITL.set("gp-init");
        Thread t13 = new Thread(() -> {
            try {
                Thread gt = new Thread(() -> gcBox.set(String.valueOf(ITL.get())));
                gt.start(); gt.join();
            } catch (Exception e) { gcBox.set("EX " + e); }
        });
        ITL.set("gp-after");
        check("grandchild", childSees(t13, gcBox), "gp-init");

        System.out.println("RJdkInheritableTL: " + checks + " checks, " + failed + " failed");
    }
}
```

Every expected value above is the **MEASURED** oracle answer from §2, not a
derivation. Rows 5, 9, 11 and 12 are the ones a plausible-looking fix gets
wrong.

Row 14 of §2 (`StructuredTaskScope.fork`) is deliberately **not** in this
vector: it needs `--enable-preview` and belongs with the JEP 505 fixtures.
The §5.2 change is what it would test:

```java
// requires --enable-preview
ITL.set("scope-parent");
try (var scope = StructuredTaskScope.open()) {
    var st = scope.fork(() -> String.valueOf(ITL.get()));
    scope.join();
    check("stsForkSubtask", st.get(), "scope-parent");   // was: null
}
```

---

## 8. What I could not settle, and what would settle it

* **Whether the `--jdk-only` binary agrees with §4.** The chain is
  `set_category(Intrinsic)` at `phases_early.rs:3683` plus the `Intrinsic`
  arm at `vm_exec.rs:849`. One run settles it:
  `--dump-native-registry` and look at `java/lang/ThreadLocal` `get`/`set` —
  `owns_slot=true` with non-zero `invocations` under `--jdk-only` confirms
  it; `invocations=0` falsifies §4 and this whole record needs re-reading.
* **What the "minimal repro" behind the original note really saw.** §3 rules
  out its stated mechanism; it does not tell us what was actually observed.
  The repro was not preserved. The likeliest candidate is an
  identity-hash-keyed queue miss in `tl_inherited_pending` rather than
  anything constructor-shaped, but that is a **guess and is labelled as
  one** — the pending queue is keyed by `identity_hash_code(child)` at
  `start0` and drained by `identity_hash_code(current_thread_object())` in
  the child, and nothing in this lane verified that those two agree across a
  moving GC. A probe that prints both hashes would settle it in one run.
* **The effective `NativeKind` of the nine `Thread.<init>` registrations**
  (N5). `--dump-native-registry`'s `kind` column.
* **Whether `native_sts_fork` runs at all under `--jdk-only`.** Its registrar
  sets `NativeKind::Bridge` (`jdk25_concurrency.rs:2077`), so if
  `StructuredTaskScope.fork` has real bytecode, §7 step 3 sends `--jdk-only`
  to that bytecode and **the §5.2 fix is `Compatible`/synthetic-only**. This
  is the honest limit of what landed.

**The blunt summary.** Under `--jdk-only` this lane changed a comment, a
policy-neutral doc block, and added a gate that today has no caller. Per
HANDOFF §5 that is a change that compiles and does nothing — *by design*,
because the alternative (§6 N3) is a wrapper on the master constructor that a
lane which cannot run the binary must not land. The `--jdk-only` fix is
**N1**, and it needs a build.

---

## 9. Reproducing the oracle table

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
"$JAVA_HOME/bin/javap" -p -c java.lang.Thread > thread.txt   # section 3
"$JAVA_HOME/bin/java" ItlProbe.java                          # section 2, rows 1-13
"$JAVA_HOME/bin/java" --enable-preview StsItl.java           # section 2, rows 14-15
```

`ItlProbe` is §7's vector with `check(...)` replaced by `println`; `StsItl` is
the two-case snippet at the end of §7. Both print ASCII only.
