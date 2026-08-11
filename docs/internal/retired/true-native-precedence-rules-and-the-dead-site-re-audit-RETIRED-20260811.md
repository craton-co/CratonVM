# The true native-vs-bytecode precedence rules, and the re-audit of every site this campaign called dead — RETIRED 2026-08-11

**Retired because every prediction in it was executed.** The record was filed
2026-08-07 by a lane that could not build or run the VM: its HotSpot numbers
were measured, its CratonVM columns were predictions read off Rust source, and
its five snippets existed so somebody could later turn those predictions into
measurements. That has now happened, on Azure linux against
OpenJDK 25.0.4+7, in **both** `--real-jdk` and `--jdk-only`.

The §1 decision procedure and the §2 re-audit table were correct as analysis and
are preserved below the line. What retires the record is that its open question
— "is the prediction true?" — has an answer for every row, and each answer now
lives in a vector that runs on every suite invocation rather than in a snippet
nobody had run.

---

## What the snippets became

The audit's five snippets are now four scheduled vectors plus two pre-existing
ones. A snippet that ends in an `EXPECT-REAL:` comment is a measurement waiting
for a reader; `regression-suite/run.sh` diffs CratonVM against HotSpot on every
run, so the same assertion becomes a gate.

| snippet | disposition |
|---|---|
| **A1** `StampedLock` | `regression-suite/src/RJdkStampedStamps.java` — 48 checks |
| **A2** `Phaser` | already covered by `RJdkPhaser` (240 checks, `getParent`/`getRoot` included) |
| **A3** `Lookup.in` / `dropLookupMode` | `regression-suite/src/RJdkLookupIn.java` — 42 checks |
| **A4** `VarHandle` | already covered by `RJdkHandles` (`findVarHandle`, `varType`, the access modes) |
| **A5** `ClassLoader.defineClass1/2` | `regression-suite/src/RJdkDefineClass.java` — 40 checks |
| *(new)* row 7's per-triple check | `regression-suite/src/RJdkX509Intercept.java` — 22 checks |

All four new vectors PASS byte-identical to HotSpot in `--real-jdk` and
`--jdk-only`.

## Row by row, prediction against measurement

### Row 1 — `StampedLock`, filed URGENT. **Stale, and refuted twice.**

The record predicted `--real-jdk` would answer `writeStamp&255 == 1`,
`isWriteLockStamp == false`, `isReadLockStamp == true` — the release-the-wrong-
lock hazard. It does not. `native-builtins/src/stamped_lock.rs` was re-encoded
to the JDK's own bit layout (`WBIT = 128`, `ORIGIN = 256`, the reader count in
the low seven bits) on **2026-08-07**, the same day this record was filed, and
the two never met.

`RJdkStampedStamps` asks through the JDK's own **unregistered** static
predicates — `isWriteLockStamp`, `isReadLockStamp`, `isLockStamp`,
`isOptimisticReadStamp`, which no registrar provides, so real JDK bytecode
decodes whatever the backend hands out. 48/48 in both modes:
`writeMode=128 readModes=1,2 optMode=0 versionStep=256`.

The record's *second* prediction for this row is also refuted: it warned that a
hang at `writeLock()` under `--jdk-only` would be "its own finding", since the
real `StampedLock` bytecode needs `Unsafe` CAS and `LockSupport.park` once the
stub is refused. It does not hang. The whole surface runs on real bytecode under
`--jdk-only` and answers identically to `--real-jdk`.

### Row 2 — `MethodHandles.Lookup.in`. **Live, and it was wrong in three ways.**

The record's charge was exact: "correctness is asserted from a comment, not a
test." Running it found three defects, all live in **both** modes, and all in
the same direction — GRANTING access the JDK withholds.

1. **`in(int.class)` and `in(String[].class)` returned a Lookup.** The JDK opens
   `in` with three rejections before any mode arithmetic — null is an NPE, a
   primitive and an array are each an `IllegalArgumentException`. A native that
   only computes modes drops all three silently. The test now lives in one
   place, `classloader::lk_check_in_target`, called by BOTH registrations of the
   method, for the same reason they already shared `lk_in_modes`.

2. **`publicLookup().in(<a package-private class>)` reported 32.** The old table
   was measured against public targets only, and read `UNCONDITIONAL` as
   surviving `in()` unconditionally. Re-measured on OpenJDK 25.0.3:

   | target | modes |
   |---|---|
   | a PUBLIC class in the unnamed module | 32 |
   | a PUBLIC nested class | 32 |
   | a package-private nested class | **0** |
   | a package-private top-level class | **0** |
   | `java.lang.String` | 32 |
   | `jdk.internal.misc.Unsafe` (public, NOT exported) | **0** |

   The class-public half is now enforced. The export half is not — there is no
   module graph — and that one divergence is recorded at the site rather than
   approximated by package prefix.

3. **A zero-mode Lookup admitted every member, public ones included.**
   `lk_read_allowed_modes` collapsed "the Lookup has no modes" and "this VM
   could not read the modes" to the same `0`, so every enforcement site had to
   treat `0` as "unknown, stay permissive". That is right for an unmodelled
   layout and wrong for `lookup().dropLookupMode(PUBLIC)`, which HotSpot refuses
   everything from. `lk_read_allowed_modes_opt` keeps them apart: the valve is
   `None`, not `0`.

   And separately, `publicLookup()`'s rule is about the target CLASS rather than
   the member, which `lk_enforce_find_access` never looked at — so a public
   member of a package-private class was reachable from `publicLookup()`.

The four `in(...)` values the record predicted (95 / 31 / 25 / 1) were right and
are now asserted; what it could not see was that the numbers being right did not
make the access decisions right.

### Row 3 — `ClassLoader.defineClass0/1/2`. **Live, and one defect, in all six paths.**

The record called the risk correctly: "not shadowing but decode fidelity". The
`bb_define_layout` fix it worried about had held — the sliced, windowed and
direct `ByteBuffer` arms all decode correctly — but the **caller's
`ProtectionDomain` never reached the class**. Every defined class came back
carrying the synthesised `file:/runtime-defined/<name>.class` code source
instead of the one the caller passed.

Six copies of an inline decode stood in four files, and every one of them read
`CodeSource.location` with `read_string`, which fails on a real `java.net.URL`
because a URL is a different concrete class — one of the copies even carried the
comment `// Try CodeSource.location at field 0 (URL object) → URL.toString()`,
describing an intent the code did not implement. The correct reader,
`classloader::extract_pd_code_source_url`, already existed and no call site used
it. All six now do.

### Row 4 — `Phaser`. **Dead, as predicted, and it was already a measurement.**

`RJdkPhaser` asserts `getParent()`/`getRoot()` directly (lines 283-286) and has
since before this record; the natives are dead in every mode the suite runs.

### Row 5 — `VarHandle`. **Both halves confirmed by `RJdkHandles`.**

The live `register_phase54_method_handle` surface (`varType`,
`coordinateTypes`) and the access modes `register_p59_varhandle` does not serve
are both exercised there, including `getAndAdd` on six primitive widths.

### Row 6 — `lk_in_method` / `lk_drop_lookup_mode`. **Confirmed, and the control fired.**

`dropLookupMode` has no real-mode registration, so the record proposed it as the
CONTROL: a divergence there would mean a registration nobody had found. It did
not diverge — but the HotSpot oracle corrected the *record's own arithmetic*.
`dropLookupMode`'s opening move is `oldModes & ~(modeToDrop | PROTECTED |
ORIGINAL)`, so PROTECTED comes off for EVERY argument, and
`dropLookupMode(UNCONDITIONAL)` on a 95 lookup is **27**, not the 31 the
`EXPECT-REAL` line predicted. Five of the six values in that line were right;
the sixth was a model, not a measurement.

### Row 7 — `X509Certificate`. **Verdict survives; now measured per triple.**

The record asked for a per-triple `javap` of `sun.security.x509.X509CertImpl`
and got one. Seventeen triples are registered on the ABSTRACT
`java.security.cert.X509Certificate`; **sixteen are declared by `X509CertImpl`
itself**, so its own bytecode wins and `has_own_bytecode` skips the superclass
walk entirely. The seventeenth is `getType()`, which is `public final` on
`java.security.cert.Certificate` two frames up — so `X509CertImpl` declares
nothing and the native DOES intercept. It answers the constant `"X.509"`, which
is exactly what `Certificate.getType()` returns for every X.509 certificate
(`X509CertImpl`'s constructor passes it to `super`). Benign — but benign by
measurement now, which is what the record asked for. `RJdkX509Intercept` parses
a fixed self-signed cert and pins all seventeen.

## Two things found while measuring that are NOT this record's

* **A duplicate `defineClass` in one loader does not raise `LinkageError`.**
  HotSpot does; CratonVM serves the already-defined mirror
  (`lang_system::same_loader_already_defined_mirror`), which is a deliberate
  tolerance for delegation gaps. Changing it touches every loader-stacking
  workload, so it is filed separately rather than folded in here.
  `RJdkDefineClass` carries the case written out and commented NOT ASSERTED,
  with the reasoning, so it is not lost.
* **`Lookup.in` for a cross-module target whose package is not exported** is 0
  on HotSpot and 1 here. Module-graph-dependent; recorded at
  `classloader::lk_in_modes` rather than approximated.

## What the record got right that is worth keeping

Its generalisation, stated in "What this re-audit did NOT find", is the durable
part and it was borne out again here:

> The one urgent finding was missed for a different reason entirely: a lane
> asked "which registrar wins?" and stopped before "and is the winner right?".

Row 2 is the same shape one level further in. A previous lane asked "does `in()`
compute the right modes?", answered yes, and stopped before "and does anything
enforce them?". Three of this session's four fixes live in that gap.

---

The original record follows unchanged.

---

# The true native-vs-bytecode precedence rules, and the re-audit of every site this campaign called dead

Filed 2026-08-07, JDK-only wave 2, lane W8-7. Source-verified only — this lane
could not build or run the VM. Every Java oracle below was measured on
**OpenJDK 25.0.3+9-LTS (HotSpot)** on the lane host; every CratonVM prediction is
read off the Rust source and is marked as a *prediction*, not a measurement.

Background mechanism facts live in
[*Natives over real JDK classes*](../../architecture/natives-over-real-jdk-classes.md)
(§1 reachability, §2 feature-vs-mode, §3 last-registration-wins). This record
does not restate them. It adds the two things that page's §1 does not carry —
a **decision procedure with an ordering**, and the **re-audit** it implies —
plus one path §1 does not mention.

---

## 0. W7-16's claim: verified, with one correction and one addition

W7-16 said the campaign's "four doors" reachability rule was wrong in the
too-restrictive direction. **Confirmed, all four sub-claims, from the source.**

| W7-16 sub-claim | Verdict | Evidence |
|---|---|---|
| Registration itself is the gate on cold interpreter paths | **TRUE** | `native_override.rs:6980` `resolve_step1_native` passes `compat_native_wins` literal `true` (line 7080); `invoke.rs:2968` calls it as step 1's PRIMARY lookup, keyed on the receiver's own class, *before* anything asks whether that class has `Code` (the `has_own_bytecode` test at `invoke.rs:3043-3049` guards only the superclass walk, three `.or_else` arms later) |
| The force list and `check_override` *reinstate*, they do not *grant* | **TRUE** | `dispatch_virtual.rs:741-747` — the warm vtable path's only registry consultation is `force_native_over_real_jdk_bytecode`; `vm_exec.rs:24019-24025` — the `check_override` chain's outcome is passed as `compat_native_wins`, i.e. the site's pre-existing verdict, not a new one |
| Door (b), `NativeKind::Intrinsic`, does not exist | **TRUE** | `vm_exec.rs:629` `resolve_dispatch`; its only non-test caller is `invoke.rs:3452`, inside `if is_native {` (`invoke.rs:3419`); step 1 (`vm_exec.rs:641-657`) returns unconditionally for `method.is_native()`, so steps 2–4 (lines 659-710) execute only from `vm/tests/jdk_only_dispatch.rs` |
| `NativeKind` only subtracts | **TRUE**, and the subtraction is bigger than stated | `vm_exec.rs:800-807` — Compatible discards the kind; `vm_exec.rs:794` — `Intrinsic` only buys exemption from a yield that is **off by default** (`native_override.rs:7038`, `jdk_only_enforce_shadow`). The real subtraction is at *registration* time: `registry.rs:5611-5639` refuses every `SyntheticStub` under `JdkOnly` |

**Correction.** W7-16 puts `vm_exec.rs`'s `check_override` chain on the
"reinstate" side. That is right about the chain, but the chain is not that
path's last word. When it declines, `invoke_on_class_shared_inner` falls to the
bytecode arm, and that arm ends at a **second, unconditional** registry lookup:

```rust
// vm/src/vm/vm_exec.rs:24595-24622
let override_cb = if declaring_is_interface && !is_static && !force_… { None }
    else if synthetic_stub_should_yield_to_real_bytecode(…) { None }
    else { shared.natives.native_methods.find(&class_name_for_override, method_name, descriptor) };
if let Some(callback) = override_cb { safe_native_call(…) } else { interpreter::execute(…) }
```

`class_name_for_override` is the **declaring** class (`vm_exec.rs:24498-24505`).
`vm_exec.rs:9731-9738` states the consequence outright: *"it has a SECOND,
unconditional native-registry check for any non-interface declaring class …
that re-finds this exact native regardless of the first gate"*. So the
reflective / `ctx.invoke_virtual` / lambda-method-ref / JNI path is also
"registration is the gate", with exactly two vetoes.

**Addition.** There is a gate *before* every door, and it is the one that
actually decides most of this campaign's "dead" verdicts — see §1 step 0.

---

## 1. The decision procedure

Answer in order. The first three questions are about the *build and the mode*;
only if all three pass does any dispatch-path question matter.

### Step 0 — is the registrar reachable in this mode?

`vm_init.rs:1552` opens `#[cfg(feature = "synthetic-jdk")] { if config.use_synthetic_jdk { … } else { … } }`,
and `#[cfg(not(feature = "synthetic-jdk"))]` at `vm_init.rs:2128` is the default
CLI build's arm. **Only the synthetic arm calls `register_builtins`**
(`vm_init.rs:1556`), and `register_builtins` is the only caller of
`register_synthetic_overrides` (`native-builtins/src/lib.rs:21041`), which is the
only caller of the whole `register_phase50..72_natives` family.

> **A registrar reachable only from `register_synthetic_overrides` cannot see a
> real JDK receiver, in any build, ever.** No dispatch rule can rescue it.

The real-JDK-reachable registrar set is the explicit list in the two real arms
(`vm_init.rs:1623-2126` and `2128-~2794`) plus everything transitively called by
`register_essential_natives_with_shims` (`native-builtins/src/lib.rs:6715`).
`vm/src/native/builtins.rs:29` supplies a **no-op** `register_synthetic_overrides`
when the feature is off, so a feature-off build cannot even reach it by accident.

### Step 1 — does the mode refuse the registration?

Three refusals run inside `NativeMethodRegistry::register` (`registry.rs:5590`):

| Gate | Condition | Effect |
|---|---|---|
| `--jdk-only` | `compatibility_mode == JdkOnly && kind == SyntheticStub` (`registry.rs:5611`, `registry.rs:4478`) | registration **refused**, recorded as `SyntheticNativeRegistered` with the `#[track_caller]` site |
| `CRATONVM_NO_STUBS` | `drop_synthetic_stubs && kind == SyntheticStub` (`registry.rs:5644`) | dropped silently (opt-in only; deliberately **not** set by real-JDK mode) |
| real-JDK layout drop | `drop_real_layout_synthetic && class ∈ {StringJoiner, EnumSet, StringReader, StringWriter, Pattern/Matcher, …}` (`registry.rs:5897-6381`) | dropped so the real self-contained bytecode runs |

> **This is the one axis that makes `--real-jdk` and `--jdk-only` disagree about
> which implementation runs.** A `SyntheticStub` registrar called from a
> real-JDK arm is **live under `--real-jdk` and dead under `--jdk-only`**.
> `register_stamped_lock_natives` is exactly that shape (§2, row 4).

### Step 2 — which path is this call taking?

Given a registration that survived steps 0 and 1, for the triple
`(C, m, d)` where `C` is the **declaring** class the invoke resolves to:

| Path | Entry point | What decides |
|---|---|---|
| **Cold interpreter**, exact triple | `invoke.rs:2968` → `resolve_step1_native` (`native_override.rs:6980`) | **Registration wins.** `compat_native_wins = true` is a literal. Three vetoes run *after*: JVMTI-redefine suppression (`invoke.rs:3101`), `synthetic_stub_should_yield_to_real_bytecode` (`invoke.rs:3108`), and — under `--jdk-only` with `CRATONVM_ENFORCE_NATIVE_SHADOW=1` only — the §7 step-3 yield |
| **Cold interpreter**, inherited method | `invoke.rs:3015-3095` superclass walk | Skipped entirely when the dispatch class declares the method (`has_own_bytecode`, `invoke.rs:3043`). Otherwise a native on an abstract ancestor **does** intercept the subclass |
| **Cold interpreter**, method with `Code` reached via the cached-invoke arm | `invoke.rs:3558-3633` | Registration wins, with `compat_native_wins = !synthetic_stub_should_yield_to_real_bytecode`. **Exception:** interface-declared instance methods are dropped unless force-listed (`invoke.rs:3550`) |
| **Warm / vtable-cached** | `dispatch_virtual.rs:741-779` | **Force list only.** A `CachedBytecodeMethod` asks the registry solely through `force_native_over_real_jdk_bytecode`; a registration not on that list is invisible once the site is warm |
| **Reflective / `ctx.invoke_virtual` / lambda method-ref / JNI** | `vm_exec.rs::invoke_on_class_shared_inner` (`vm_exec.rs:19996`) | Two chances: the ~302-disjunct `check_override` chain (`vm_exec.rs:20495`) runs the native early; if it declines, `override_cb` (`vm_exec.rs:24595`) runs it late, unconditionally, vetoed only by the interface-default guard and the stub yield |
| **`ACC_NATIVE` JDK method** | `invoke.rs:3419-3502` → `resolve_dispatch` step 1 | Registration answers or the call falls through to JNI. `Intrinsic` vs `Bridge` is indistinguishable here; only `SyntheticStub` under `--jdk-only` is refused |
| **JIT by-name fast paths** | `jit/helpers.rs:7674` `admit_jit_fast_native` | A hard-coded fast-path table, receiver-class guarded; `resolve_native_dispatch_wave1` is asked with `compat_native_wins = true` (`helpers.rs:7646`) |

### Step 3 — only now, does a list matter?

`force_native_over_real_jdk_bytecode` matters in exactly two situations:
**(a)** the call site is warm/vtable-cached, and **(b)** the declaring class is an
interface and the call is an instance call. Nowhere else does it add reach it did
not already have. `NativeKind::Intrinsic` never adds reach at all.

### The cold/warm divergence is a real defect species, not a curiosity

Because the cold path's gate is registration and the warm path's gate is the
force list, a triple that is registered but not force-listed **changes behaviour
with call-site temperature**. Two in-tree records name concrete occurrences:
`invoke.rs:3526-3542` (an interface bridge materialised an EMPTY stream on the
first call at each site and the real bytecode ran on every later one —
Hibernate `JoinedList`), and `native_override.rs:6497-6504` (a `SyntheticStub`
yield verdict that depended on how many times the site had executed —
`java/util/StringJoiner`). Any new "should this native win" answer must be given
for both temperatures or it is half an answer.

---

## 2. The re-audit

"Old verdict" is what a lane recorded. "Survives?" is against the procedure
above. Ranked by consequence: **live and wrong** first.

| # | Site | Old verdict | Survives? | Consequence |
|---|---|---|---|---|
| 1 | `native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives` — 25 `StampedLock` triples + 6 view triples | *"needs no change — the losing registrar was disabled"* (`W6-12-stampedlock-split-brain.md`) | **NO — the question was never asked.** W6-12 settled *which of two registrars wins*. It did not ask whether the winner agrees with the JDK. The winner is registered on the **essential** path (`lib.rs:16877`) and **twice more** from the real-JDK arms (`vm_init.rs:1690`, `2169`), and is **force-listed** (`native_override.rs:2034-2076`), so it wins on every path in `--real-jdk`. It is tagged `SyntheticStub` (`util_concurrent_ext.rs:5977`), so under `--jdk-only` it is **refused at registration** and the real bytecode runs | **URGENT — live and wrong under `--real-jdk`, and mode-divergent.** The native's stamp encoding is `version\|1` for write and `version\|2` for read (`stamped_lock.rs:83-92`); the JDK's is `WBIT = 128`. The JDK's own `isWriteLockStamp` / `isReadLockStamp` / `isLockStamp` statics are **not registered**, so they run real bytecode over a foreign stamp. Measured HotSpot: write stamp `&255 == 128`, `isWriteLockStamp == true`, `isReadLockStamp == false`. Predicted CratonVM `--real-jdk`: `1`, `false`, **`true`** — so the idiom `if (isWriteLockStamp(s)) unlockWrite(s); else unlockRead(s);` releases the wrong lock and the write hold is never dropped. Snippet **A1** |
| 2 | `native-builtins/src/lang_invoke.rs::register_p63_method_handles_lookup` — `MethodHandles$Lookup.in` | *"wins in different copies per mode"* | **YES, and the mode split is the whole point.** `classloader.rs:9472` (`lk_in_method`) is reachable only from `register_classloader_natives` ← `register_synthetic_overrides` → synthetic-only. `lang_invoke.rs:4260` is in `register_p63_method_handles_lookup`, called from **both** real arms (`vm_init.rs` real arms) → **live under `--real-jdk` and `--jdk-only`**, shadowing concrete JDK bytecode | **HIGH — live, and correctness is asserted from a comment, not a test.** The registration's own comment claims a four-value reduction measured on OpenJDK 25. Independently re-measured here: `lookup()` 95, `in(self)` 95, `in(nestmate)` 31, `in(other module)` 1, `publicLookup()` 32. A mismatch is an access-control error, not a wrong number. Snippet **A3** |
| 3 | `ClassLoader.defineClass0/1/2` shadowing | *"synthetic-jdk mode only"* | **NO — half wrong.** True of `classloader.rs:4366` `register_classloader_define_class` (← `register_classloader_natives` ← `register_synthetic_overrides`). **False** of `lang_system::native_classloader_define_class0/1/2`, registered `Bridge` on the **essential** path at `native-builtins/src/lib.rs:15793-15814` — live in every mode | **MEDIUM — live, and load-bearing rather than wrong.** These three are `ACC_NATIVE` in the real JDK (door 1), so there is no bytecode to shadow; the native *is* the implementation. The risk is not shadowing but decode fidelity — this is where W4-4's MISMATCH 1 (`ByteBuffer` slot-0 read) lived. Snippet **A5** |
| 4 | `native-builtins/src/phases_early.rs::register_phaser_natives` — 12 `Phaser` triples | *"dead under `--real-jdk`/`--jdk-only`"* | **YES, and for a reason the four-door rule never supplied.** Chain: `register_phaser_natives` (`phases_early.rs:7814`) ← `register_phase51_natives` (`phases_early.rs:6588`) ← `register_synthetic_overrides` (`lib.rs:23478`) ← `register_builtins` ← the synthetic arm only. Step 0 kills it before any door is consulted | **LOW — genuinely dead.** But the *stated reason* in `phases_early.rs:7796-7814` ("no `Phaser` method is `ACC_NATIVE`, real `Phaser` declares `Code` for all of…") is a four-door argument and would not, on its own, have been sound. Snippet **A2** exists to prove the dead verdict rather than assume it — the natives park an `int[3]` in slot 1, which is the real `parent` field, so a live native is loudly visible |
| 5 | `native-builtins/src/phases_late/reflect_invoke.rs::register_p59_varhandle` | *"dead"* | **YES.** ← `register_phase59_natives` (`phases_late.rs:1848`) ← `register_synthetic_overrides` (`lib.rs:23519`). But the *inference* several records draw from it — "VarHandle natives cannot intercept a real `VarHandle`" — is **false**: `register_phase54_method_handle` (`lang_invoke.rs:638`, live in both real arms) registers `varType`, `coordinateTypes`, `withInvokeExactBehavior`, `withInvokeBehavior`, `accessModeTypeUncached` on the same class | **LOW for p59; MEDIUM for what replaced it.** The live VarHandle surface is `lang_invoke.rs`'s, and it answers from the `VarHandleMeta` side table. Snippet **A4** exercises both the metadata (live natives) and the access modes (p59's dead surface) |
| 6 | `native-builtins/src/classloader.rs` `lk_in_method` / `lk_drop_lookup_mode` | *"latent, not live"* | **YES for this file.** Both sit in `register_classloader_natives` (`classloader.rs:9142`), reached only from `register_synthetic_overrides`. `dropLookupMode` has **no** competing real-mode registration anywhere (`grep '"dropLookupMode"'` → only `classloader.rs`) | **LOW.** `dropLookupMode` runs real bytecode in real-JDK mode. `in` does **not** — see row 2. Filing them as one verdict was the error; they have different answers. Snippet **A3** covers both |
| 7 | `W4-4-slot-index-species-sweep.md` rows: `ProcessBuilder`, `java/sql/*`, `X509Certificate`, `AsynchronousChannel` | *"safe — has `Code`, not force-listed"* | **Verdicts survive; reasons do not.** Corrected in place in that record. `ProcessBuilder` is reachable and safe only because its slot indices happen to match. `java/sql/*` and `AsynchronousChannel` are safe because the **declaring class is the impl**, not because of any list. `X509Certificate` is safe **only for the triples `X509CertImpl` declares** — an inherited one would be caught by the step-1 superclass walk | **MEDIUM (`X509Certificate` only) — unverified.** Needs a per-triple `javap` check of `sun.security.x509.X509CertImpl` |

### What this re-audit did NOT find

No site in the known-candidate list turned out to be live *because* the four-door
rule was too restrictive. Steps 0 and 1 dispose of rows 4, 5 and 6 outright, and
they would have disposed of them under the old rule too — by luck, since the old
rule's reasoning was unsound. **The one urgent finding (row 1) was missed for a
different reason entirely: a lane asked "which registrar wins?" and stopped
before "and is the winner right?".** That is the pattern worth generalising from
this lane, not the four doors.

---

## 3. The snippets

Each runs on a bare `--jdk-only` (or `--real-jdk`) VM with no fixtures, no
classpath and no arguments: `cratonvm --jdk-only AuditX.java`, or compile with
`javac` first. Each prints only primitives and ends with an `EXPECT-REAL:` line
carrying the **measured HotSpot 25.0.3 answer**. Divergence from that line is the
finding; the per-snippet note says what each divergence proves.

Run each under **both** `--real-jdk` and `--jdk-only` — rows 1 and 3 predict
different answers per mode, and a single-mode run cannot see that.

### A1 — `StampedLock` stamp encoding (row 1, URGENT)

```java
import java.util.concurrent.locks.StampedLock;

public class AuditSL {
    public static void main(String[] a) {
        StampedLock l = new StampedLock();
        long w = l.writeLock();
        System.out.println("writeStamp&255      = " + (w & 255L));
        System.out.println("isWriteLockStamp(w) = " + StampedLock.isWriteLockStamp(w));
        System.out.println("isReadLockStamp(w)  = " + StampedLock.isReadLockStamp(w));
        l.unlockWrite(w);
        long r = l.readLock();
        System.out.println("readStamp&255       = " + (r & 255L));
        System.out.println("isReadLockStamp(r)  = " + StampedLock.isReadLockStamp(r));
        l.unlockRead(r);
        System.out.println("EXPECT-REAL: 128 / true / false / 1 / true");
    }
}
```

* `128 / true / false / 1 / true` → the natives are **not** intercepting; real
  `StampedLock` bytecode ran. Expected under `--jdk-only` (registration refused).
* `writeStamp&255` is **1** and `isWriteLockStamp(w)` is **false** → the
  `util_concurrent_ext` natives are live and their stamp encoding diverges.
  Predicted under `--real-jdk`. `isReadLockStamp(w) == true` on the same run is
  the release-the-wrong-lock hazard, stated.
* Any other low byte → a third implementation is in play; find it before
  concluding anything.
* A hang at `writeLock()` under `--jdk-only` is its own finding: the real
  `StampedLock` bytecode needs `Unsafe` CAS + `LockSupport.park`, and refusing
  the stub would have exposed a gap, not fixed one.

### A2 — `Phaser` (row 4: proves the dead verdict)

```java
import java.util.concurrent.Phaser;

public class AuditPhaser {
    public static void main(String[] a) {
        Phaser p = new Phaser(2);
        System.out.println("getParent()          = " + p.getParent());
        System.out.println("getRoot()==this      = " + (p.getRoot() == p));
        System.out.println("registered           = " + p.getRegisteredParties());
        System.out.println("phase(before arrive) = " + p.getPhase());
        p.arrive();
        System.out.println("arrived              = " + p.getArrivedParties());
        System.out.println("unarrived            = " + p.getUnarrivedParties());
        p.arrive();
        System.out.println("phase(after 2)       = " + p.getPhase());
        System.out.println("arrived(after 2)     = " + p.getArrivedParties());
        System.out.println("EXPECT-REAL: null / true / 2 / 0 / 1 / 1 / 1 / 0");
    }
}
```

`getParent()` and `getRoot()` are **not** registered, so they always read the
real fields. The 12-triple native stores its `int[3]` state holder in field slot
**1**, which is real `Phaser.parent`.

* `null / true / …` → the natives are dead, as predicted. This is the expected
  result and it is what makes the dead verdict a measurement.
* `getParent()` non-null (or a `ClassCastException` on the `[I`) → the natives
  are LIVE in this configuration, step 0 is wrong for this registrar, and the
  `Phaser` accessors are reading a fabricated 3-int state over the real 5-field
  layout.

### A3 — `Lookup.in` and `dropLookupMode` (rows 2 and 6)

```java
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;

public class AuditLookup {
    static class Nested {}

    public static void main(String[] a) {
        Lookup l = MethodHandles.lookup();
        System.out.println("lookupModes                     = " + l.lookupModes());
        System.out.println("in(AuditLookup.class)           = " + l.in(AuditLookup.class).lookupModes());
        System.out.println("in(Nested.class)                = " + l.in(Nested.class).lookupModes());
        System.out.println("in(String.class)                = " + l.in(String.class).lookupModes());
        System.out.println("publicLookup()                  = " + MethodHandles.publicLookup().lookupModes());
        System.out.println("publicLookup().in(AuditLookup)  = "
                + MethodHandles.publicLookup().in(AuditLookup.class).lookupModes());
        System.out.println("dropLookupMode(PRIVATE)         = " + l.dropLookupMode(Lookup.PRIVATE).lookupModes());
        System.out.println("dropLookupMode(PACKAGE)         = " + l.dropLookupMode(Lookup.PACKAGE).lookupModes());
        System.out.println("dropLookupMode(MODULE)          = " + l.dropLookupMode(Lookup.MODULE).lookupModes());
        System.out.println("dropLookupMode(PUBLIC)          = " + l.dropLookupMode(Lookup.PUBLIC).lookupModes());
        System.out.println("EXPECT-REAL: 95 / 95 / 31 / 1 / 32 / 32 / 25 / 17 / 1 / 0");
    }
}
```

The four `in(...)` lines exercise the **live** `lang_invoke` native (row 2); the
four `dropLookupMode` lines exercise a method with **no** real-mode registration
(row 6), so they are the control. If `dropLookupMode` diverges, a registration
exists that this audit did not find — that is a bigger finding than any `in`
mismatch. If only `in(...)` diverges, the `lang_invoke` reduction table is wrong
and it is an access-control defect.

### A4 — `VarHandle` (row 5)

```java
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class AuditVH {
    int n = 7;
    volatile Object o = "a";

    public static void main(String[] x) throws Throwable {
        AuditVH t = new AuditVH();
        VarHandle vn = MethodHandles.lookup().findVarHandle(AuditVH.class, "n", int.class);
        VarHandle vo = MethodHandles.lookup().findVarHandle(AuditVH.class, "o", Object.class);
        System.out.println("varType(n)          = " + vn.varType());
        System.out.println("coordinateTypes     = " + vn.coordinateTypes());
        System.out.println("get                 = " + (int) vn.get(t));
        vn.set(t, 9);
        System.out.println("after set           = " + t.n);
        System.out.println("cas 9->11           = " + vn.compareAndSet(t, 9, 11));
        System.out.println("after cas           = " + t.n);
        System.out.println("getAndAdd(+5)       = " + (int) vn.getAndAdd(t, 5));
        System.out.println("after getAndAdd     = " + t.n);
        System.out.println("getVolatile(o)      = " + vo.getVolatile(t));
        vo.setRelease(t, "b");
        System.out.println("after setRelease    = " + t.o);
        System.out.println("EXPECT-REAL: int / [class AuditVH] / 7 / 9 / true / 11 / 11 / 16 / a / b");
    }
}
```

Lines 1-2 are the **live** `register_phase54_method_handle` surface; lines 3-10
are the access modes `register_p59_varhandle` would have served and does not.
Divergence on lines 1-2 is a live-native defect. Divergence or
`UnsupportedOperationException`/`AbstractMethodError` on lines 3-10 is the
*absence* of p59 becoming visible — that is a gap, not a shadow, and the remedy
is different.

### A5 — `ClassLoader.defineClass1` / `defineClass2` (row 3)

```java
import java.security.ProtectionDomain;
import java.security.CodeSource;
import java.security.cert.Certificate;
import java.nio.ByteBuffer;
import java.net.URL;

public class AuditDefine {
    static byte[] tiny(String name) {          // a valid, field-less, method-less class
        java.io.ByteArrayOutputStream b = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream d = new java.io.DataOutputStream(b);
        try {
            d.writeInt(0xCAFEBABE);
            d.writeShort(0); d.writeShort(52);
            d.writeShort(5);
            d.writeByte(7); d.writeShort(2);
            d.writeByte(1); d.writeUTF(name);
            d.writeByte(7); d.writeShort(4);
            d.writeByte(1); d.writeUTF("java/lang/Object");
            d.writeShort(0x0021);
            d.writeShort(1); d.writeShort(3);
            d.writeShort(0); d.writeShort(0); d.writeShort(0); d.writeShort(0);
        } catch (Exception e) { throw new RuntimeException(e); }
        return b.toByteArray();
    }

    static class L extends ClassLoader {
        Class<?> viaArray(String n) { byte[] b = tiny(n); return defineClass(n, b, 0, b.length); }
        Class<?> viaBuffer(String n) {
            byte[] b = tiny(n);
            ProtectionDomain pd = new ProtectionDomain(new CodeSource(url(), (Certificate[]) null), null);
            return defineClass(n, ByteBuffer.wrap(b), pd);
        }
        Class<?> viaSlicedBuffer(String n) {   // non-zero ByteBuffer.offset — W4-4 MISMATCH 1
            byte[] b = tiny(n);
            byte[] padded = new byte[b.length + 8];
            System.arraycopy(b, 0, padded, 8, b.length);
            ByteBuffer bb = ByteBuffer.wrap(padded);
            bb.position(8);
            return defineClass(n, bb.slice(), null);
        }
        static URL url() { try { return java.net.URI.create("file:/audit").toURL(); } catch (Exception e) { return null; } }
    }

    public static void main(String[] a) throws Exception {
        L l = new L();
        System.out.println("array  name         = " + l.viaArray("Zz1").getName());
        System.out.println("array  loader==L    = " + (l.viaArray("Zz1b").getClassLoader() == l));
        Class<?> c2 = l.viaBuffer("Zz2");
        System.out.println("buffer name         = " + c2.getName());
        System.out.println("buffer codesource   = " + c2.getProtectionDomain().getCodeSource().getLocation());
        Class<?> c3 = l.viaSlicedBuffer("Zz3");
        System.out.println("slice  name         = " + c3.getName());
        System.out.println("slice  super        = " + c3.getSuperclass().getName());
        System.out.println("EXPECT-REAL: Zz1 / true / Zz2 / file:/audit / Zz3 / java.lang.Object");
    }
}
```

`viaSlicedBuffer` is the specific shape W4-4's MISMATCH 1 fixed: a heap
`ByteBuffer` with a non-zero `offset`. A `ClassFormatError: defineClass2: direct
ByteBuffer has no native address`, or a wrong `getName()`, means the
`bb_define_layout` fix is not in the binary under test or has regressed. A
`ClassFormatError` on `viaArray` alone points at `defineClass1` instead.

---

## 4. What would falsify this record

* **Step 0.** `CRATONVM_DBG_DROPPED_STUBS=1` plus the schema-2 census: if a
  triple from `register_phase51_natives` / `register_phase59_natives` /
  `register_classloader_natives` appears in a `--real-jdk` census at all, step 0
  is wrong and the whole §2 table has to be re-run.
* **Step 1.** A `--jdk-only` run whose refused-registration list does **not**
  contain the 31 `StampedLock` triples falsifies row 1's mode split.
  Independently corroborated in-tree: `register_stamped_lock_natives`'s own
  banner (added by lane W7-15, 2026-08-07) states *"under
  `CompatibilityMode::JdkOnly` the refusal arm returns before inserting, so all
  31 triples are pushed onto `refused` once per call"* — and that it is called
  **three times per boot in both mode arms**, so a gate counting refusal *rows*
  sees this surface three times. Count distinct triples.
* **Step 2.** `CRATONVM_DBG_STUB_YIELD=1` names the term that decided every
  allow-listed `SyntheticStub` arbitration. `StampedLock` should never appear —
  it is not on `real_protected_stub_class_common`'s twelve-class list
  (`native_override.rs:6520-6586`). If it does appear, someone added it and row 1
  is stale.
* **Every snippet.** A `--real-jdk` run and a `--jdk-only` run of the same
  snippet that agree, where §2 predicts they differ, falsifies that row.

Nothing in this record was executed against CratonVM. The HotSpot numbers were
measured; the CratonVM columns are predictions from source and are the reason
the snippets exist.
