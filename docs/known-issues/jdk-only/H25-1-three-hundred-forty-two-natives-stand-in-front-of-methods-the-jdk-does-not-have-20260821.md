# H25-1 — 342 registrations in strict mode name a method NO JDK 25 image declares, and the tree already retires this shape at CLASS granularity but not at METHOD granularity

**Status: OPEN — MEASURED.** Two `--dump-native-registry --explain-jdk-only`
dumps (one `--jdk-only`, one default/compatible) from the prebuilt
`C:/craton/cratonvm-r8.exe` (built at `025780ff7`, 42,390,016 bytes), plus
`javap` against the oracle image (Microsoft JDK 25.0.3.9-hotspot). **No source
change; no build.** Lane H25, 2026-08-21.

`H14-1` §4 split the 1402 shadows into *retire* (1244), *registered on a class
that does not declare the method* (156) and *do not touch* (2). This record
names a **fourth** verb that none of those three covers, measures it across the
whole strict-mode registry, and shows the tree already implements it — for a
different granularity.

---

## 0. The gap this worktree was cut across

`git log --oneline -1` at session start: `26e4b5db4`. `git merge --ff-only
claude/jdk-only-mode-handoff-09b48c` fast-forwarded **132 commits — 126 files,
+27,577 / −1,391 lines**. Eleven of eleven lanes before this one hit the same
cut. What was in the gap and is load-bearing here:

* `regression-suite/probes/shadow-triage.py` — the classification tool this
  lane's task is written around.
* `H14-1`, `H14-2`, `H14-3` — the classification of the 1402, the 445-row
  unclaimed block, and the thirteen priced arms.
* `H16-3` (the armed dial yields once per process), `H22-1`/`H22-2` (the
  duplicate-registration trap), `H0-8` (four false discriminators from case
  order).

Without the merge this lane would have re-derived a classification that already
existed and would not have known about the `owns_slot` trap in §4 of `H25-2`.

**One check worth recording:** `git diff 025780ff7 HEAD --` over this lane's
three owned files is **empty**. The prebuilt binary's source for
`lang_class.rs`, `lang_string.rs` and `deprecated_lang.rs` is byte-identical to
the working tree, so every registry figure below describes the source in this
worktree and not a stale ancestor. (`stale-bi`: the standing habit is to check
the binary, not assume it.)

## 1. The measurement

```text
CV=C:/craton/cratonvm-r8.exe
$CV --jdk-only --dump-native-registry reg.json        --explain-jdk-only -cp . Hello
$CV           --dump-native-registry reg_compat.json  --explain-jdk-only -cp . Hello
  # schema 4, image-adjudicated; `Hello` is a one-line println
```

| dump | mode | registrations |
|---|---|---:|
| `reg.json` | `jdk-only` | **10,378** |
| `reg_compat.json` | `compatible` | **12,093** |

`image_declaring_method` parses the real class-path bytes for **every**
registered class, so it answers for rows this workload never loaded — the
question `real_declaring_method` cannot answer (`G33-1`). Schema 4 carries
`image_has_class`, `declared`, `acc_native`, `has_code`, `inherited_from`,
`inherited_acc_native`, `inherited_has_code`, `inherited_abstract`.

### 1.1 The fourth verb

Partition the 10,378 strict-mode registrations by what the image says about the
triple:

| image verdict | registrations | note |
|---|---:|---|
| the receiver class is **absent** from the image | 999 | `H14-1`'s `no-image-receiver` shape; mostly Linux/macOS-only and third-party names |
| the method is **declared nowhere** — not on the named class, not on any supertype | **342** | **this record** |
| everything else (declared, or inherited from a supertype) | 9,037 | `H14-1` §4's three verbs |

**342 registrations name a method that no JDK 25 image declares anywhere on the
receiver's hierarchy.** They are not "the native won over bytecode" — there is
no bytecode, and there is no `ACC_NATIVE` method either. There is nothing on the
other side at all.

* **314 of the 342 own their slot** (`owns_slot: true`), i.e. they are the
  reachable registration, not a shadowed loser.
* **342 of 342 have `invocations: 0`.**
* They span **102 distinct receiver classes** and 25+ source files.

### 1.2 The positive control for that zero

A zero invocation count proves nothing on its own (`H11` retired four rows at
0 invocations while `DataInputStream.readInt` took 540 in the same runs). In
**this same dump**, from a `Hello`-world workload:

| invocations | registration |
|---:|---|
| 683 | `java/util/HashMap$KeyIterator.hasNext()Z` |
| 576 | `java/util/HashSet.iterator()Ljava/util/Iterator;` |
| 449 | `jdk/internal/util/ArraysSupport.vectorizedHashCode(...)I` |
| 410 | `java/lang/Object.<init>()V` |
| 379 | `java/util/HashSet.hashCode()I` |

**49 registrations took a non-zero count** in a program whose entire body is one
`println`. The counter is live in this run, so the 342 zeros are a real zero and
not a dead instrument.

**The zero is corroboration, not the argument.** The argument is the image: a
`invokevirtual`/`invokestatic` naming a method the image does not declare
resolves to `NoSuchMethodError` and never reaches a native. The count is what
you would expect if that is true; the image adjudication is why it is true.

**Stated caveat, from the dump's own banner:** `invocations` is a **LOWER
BOUND** — intrinsic-table and JIT direct-call dispatches are not counted. An
exact census needs `--nojit` and `CRATONVM_DISABLE_INTRINSICS=1` (`G33-1`).
This does not weaken the 342, because the image verdict does not depend on the
counter at all; it does mean **no** zero in this dump should be quoted as proof
on its own.

### 1.3 Where they are

| registrations | source file |
|---:|---|
| 30 | `native-builtins/src/lib.rs` |
| 29 | `native-builtins/src/shared_secrets_bridge.rs` |
| 22 | `native-io/src/lib.rs` |
| 21 | `native-builtins/src/unsafe_natives.rs` |
| 18 | `native-builtins/src/locale_bootstrap.rs` |
| 17 | `native-collections/src/lib.rs`, `native-builtins/src/plain_socket.rs` |
| 14 | `native-io/src/socket_channel.rs` |
| 12 | `native-io/src/file_channel.rs` |
| 11 | `native-builtins/src/phases_late/foreign_ffm.rs` |
| 10 | `native-builtins/src/streams.rs`, `deprecated_internal.rs`, `native-io/src/nio_native.rs` |
| **6** | **`native-builtins/src/deprecated_lang.rs`** ← this lane |
| **4** | **`native-builtins/src/lang_string.rs`** ← this lane |

By package: `java/lang` **70**, `jdk/internal` 57, `java/util` 56, `sun/nio` 53,
`java/nio` 46, then `java/net` and `java/security` at 10 each.

**`java/lang` is the largest block**, which places this squarely in `H14-2` §4's
**168-row unclaimed `java.lang` core** — the population this lane was pointed at
and the one no P0/P1/P2 row names.

### 1.4 The `java/lang` dead list, in full

MEASURED, 46 rows (the seven in this lane's owned files marked ★):

```
AbstractStringBuilder.repeat(Ljava/lang/String;I)…          lang_string.rs:534
StringBuilder.repeat(Ljava/lang/String;I)…                  lang_string.rs:534
StringBuffer.repeat(Ljava/lang/String;I)…                   lang_string.rs:534
★ StringUTF16.isBigEndian()Z                                lang_string.rs:12731
★ Thread.countStackFrames()I                                deprecated_lang.rs:385
★ Thread.destroy()V                                         deprecated_lang.rs:382
★ Thread.resume0()V                                         deprecated_lang.rs:379
★ Thread.suspend0()V                                        deprecated_lang.rs:378
★ Thread.stop0(Ljava/lang/Object;)V                         deprecated_lang.rs:369  (+ a losing copy at lib.rs:14063)
★ System.runFinalizersOnExit(Z)V                            deprecated_lang.rs:407
  Thread.sleep0(J)V                                         lib.rs:14024
  Object.registerNatives()V                                 lib.rs:10272
  Object.{supplier,accumulator,finisher,combiner}()…        native-collections/lib.rs:26697-26715
  Class.getProtectionDomain0()…                             lib.rs:13058
  Class.hasRealParameterData()Z                             lib.rs:12595
  Throwable.getStackTraceDepth()I                           lib.rs:15857
  Throwable.getStackTraceElement(I)…                        lib.rs:15863
  StackTraceElement.initStackTraceElements(…)V              lib.rs:17349
  StackStreamFactory$AbstractStackWalker.{callStackWalk ×3,
      fetchStackFrames ×2, checkStackWalkModes}             lang_stackwalker.rs, stack_walker.rs
  System$1.{13 SharedSecrets accessors}                     shared_secrets_bridge.rs
  ProcessBuilder$Redirect.{INHERIT,PIPE}()…                 phases_late.rs:1623,1633
  ProcessEnvironment.environ()[[B                           lib.rs:15494
  ProcessImpl.init()V                                       lib.rs:15505
  SecurityManager.getRootGroup()…                           security_manager.rs:966
```

Each of the seven ★ rows was checked against the oracle image by hand, not only
by the dump:

```
$ javap -p java.lang.Thread   | grep -Ei 'stop|suspend|resume|countStackFrames|destroy'
  public final void stop();          <- the ONLY survivor
$ javap -p java.lang.System    | grep -i runFinaliz
  public static void runFinalization();      <- runFinalizersOnExit is GONE
$ javap -p java.lang.StringUTF16 | grep -Ei 'isBigEndian|getChars'
  public static void getChars(byte[], int, int, char[], int);   <- isBigEndian is GONE
```

`Thread.stop0`, `suspend0`, `resume0`, `destroy`, `countStackFrames` and
`System.runFinalizersOnExit` were **removed from the JDK**, not deprecated. The
registrations are stubs for an API surface that stopped existing.

### 1.5 One caveat that removes 5 of the 342

Five of the 342 are registered on **`java/lang/Object`** itself
(`supplier`/`accumulator`/`finisher`/`combiner`, from `native-collections`).
A native on `java/lang/Object` is a **catch-all** and the override-chain check
admits a subclass receiver (`chk_ovr`: `check_override_chain` admits 19
families), so "Object does not declare it" does **not** imply unreachable for
those five. They are excluded from any claim of unreachability.

**337 of the 342 are on named, non-`Object` receivers** and are the population
this record is about.

## 2. The tree already does this — at CLASS granularity

The two dumps differ by 1,719 registrations, and among them are **all five
`java/lang/Compiler` registrations**, which come from **the same registrar, in
the same file, as six of this lane's seven dead rows**:

| triple | registrar | in `compatible` | in `jdk-only` |
|---|---|:-:|:-:|
| `java/lang/Compiler.compileClass` | `deprecated_lang.rs:427` | yes | **no** |
| `java/lang/Compiler.{compileClasses,enable,disable,command}` | `deprecated_lang.rs:433-441` | yes | **no** |
| `java/lang/Thread.destroy()V` | `deprecated_lang.rs:382` | yes | **yes** |
| `java/lang/Thread.countStackFrames()I` | `deprecated_lang.rs:385` | yes | **yes** |
| `java/lang/System.runFinalizersOnExit(Z)V` | `deprecated_lang.rs:407` | yes | **yes** |

`register_deprecated_lang_natives` runs unconditionally in both modes
(`lib.rs:17477`, commented *"always registered, not behind synthetic-jdk"*), so
this is not a mode gate on the registrar. The only property separating the
dropped five from the kept ten is that **`java/lang/Compiler` has no image
class at all**, while `java/lang/Thread` and `java/lang/System` do — and the
method does not.

**So strict mode already refuses a registration when the CLASS is provably
absent, and accepts the identical contradiction when only the METHOD is
provably absent.** The argument for the two is the same argument.

The tree also already holds the *reasoning* for it, in
`native-api/src/no_image_receiver.rs` — a 588-line, six-image
(Temurin 21.0.12+8 and 25.0.4+7 × linux/windows/macos) table of receiver classes
no supported image declares, re-derivable by
`scripts/jdk-only-no-image-receivers.py`, with `java/lang/Compiler` on it at
line 185. Its docstring states the rule this record extends:

> if no image on any supported (version, platform) pair declares the receiver
> *class*, there is no `ACC_NATIVE` method for the registration to bind to and
> there never can be

Replace *class* with *method* and the sentence is still true, and 342
registrations satisfy it.

### 2.1 What is NOT established about the mechanism

**This record does NOT identify the code path that drops the five
`java/lang/Compiler` registrations from the strict dump.** I checked:
`receiver_declared_by_no_supported_image` (`no_image_receiver.rs:445`) is
consulted **only by its own unit tests** — `grep -rn` over the tree returns its
definition, its own `mod tests`, and one comment in `class_manager.rs:11882`
and one in `net_phase_e.rs:19762`. So the drop is **not** performed by that
predicate, and the `NO_IMAGE_JDK_RECEIVERS` table is a *tagging* table whose
runtime consumer I did not find. The drop is measured; its mechanism is
**ARGUED to be class-existence-shaped and NOT traced to a line.** Anyone
building on §2 must find that line first — if the drop turns out to come from a
different cause, the "already does this at class granularity" framing weakens to
"already achieves this at class granularity", and the nomination in §4 still
stands on the image argument alone.

## 3. Why this is worth more than its row count

The 342 are **not** in the 1402. A `native-shadows-bytecode` row is recorded
when a native runs *instead of bytecode*; these have no bytecode to run instead
of, so they never produce a shadow row and **the census cannot see them**. That
is the point:

* **They are invisible to every instrument the effort currently uses.** `H14-1`
  classified the 1402. `H14-3` priced thirteen registrars with the armed dial.
  Neither can see a registration that is never dispatched.
* **They cost nothing to remove and clear nothing from the census.** A lane
  that retires them and reports a census delta will report **zero**, which reads
  as "no effect" and is the exact misreading `H14-1` §5 warns about for the 162
  multi-registered triples. **Predicted census delta for retiring all 342:
  0. That is a PASS, not a failure.**
* **They are a standing fabrication risk, not dead weight.** `H0-6` (*the
  fabrication surface is growing*) and `Ok≠use` / `name≠real` are about the VM
  fabricating a carrier for a name it was asked for. A registration for a method
  the image does not declare is a name the VM will answer to. It cannot be
  reached by ordinary bytecode — but it is reachable by anything that dispatches
  by name rather than by resolution, and `H4-1` measured **168 direct Rust
  calls that bypass the registry** entirely.

## 4. What this does NOT establish

* **The mechanism of the class-granular drop is not traced** (§2.1). It is the
  single largest hole in this record.
* **"Unreachable" is ARGUED from resolution semantics**, not demonstrated by a
  probe. I did not write a vector that calls `Thread.destroy()` and asserts
  `NoSuchMethodError`. The image evidence is strong and the invocation counts
  agree, but `reach≠defect` cuts both ways and no probe was run.
* **The five `java/lang/Object` catch-alls are explicitly excluded** (§1.5) and
  nothing here says what should happen to them.
* **342 is a count of registrations, not of defects.** Some of these stubs may
  be load-bearing for `--synthetic-jdk`, where a fabricated carrier CAN declare
  a method the real image does not. This record measures **only** the
  `--jdk-only` dump. A method-granular gate MUST be strict-mode-only for exactly
  that reason — `flag≠mode drops it` is the standing shape.
* **No source was changed and nothing was built.** Every figure is from the
  prebuilt `025780ff7` binary.
* **`invocations` is a lower bound** (§1.2) and no zero in this dump is
  self-supporting.

## 5. NOMINATIONS

* **N1 — add a METHOD-granular strict-mode gate**, the exact analogue of the
  class-granular one §2 measures. A registration whose triple the image declares
  nowhere on the receiver hierarchy is a contradiction under
  `jdk-only-mode.md` §1.5 by the same sentence that already covers the class
  case. It retires **342 registrations with no source deletion**, so it cannot
  break a manifest test (§`H25-2`), cannot promote a losing duplicate into
  service (`H22`), and needs no per-site review. Strict mode only.
* **N2 — expect a census delta of ZERO and write that down before running it**
  (§3). Whoever lands N1 will otherwise measure nothing and conclude nothing
  happened.
* **N3 — re-derive the 342 across the six images `no_image_receiver.rs`
  already sweeps**, not just the one Windows JDK 25 image measured here. That
  file's own history is the reason: `sun/nio/ch/KQueuePort` is on neither Linux
  nor Windows and on both macOS images, and *"a two-platform sweep called it
  dead"*. `scripts/jdk-only-no-image-receivers.py` already does the sweep; it
  needs a method-granular sibling.
* **N4 — 70 of the 342 are `java/lang`**, the top package, inside `H14-2` §4's
  168-row unclaimed `java.lang` core. That block still has no P0/P1/P2 row.
* **N5 — trace the class-granular drop to a line** (§2.1) and put a comment at
  it naming this record, so the next lane does not spend an hour finding that
  `receiver_declared_by_no_supported_image` has no runtime caller.
* **N6 — `Thread.sleep0(J)V`, `Object.registerNatives()V`,
  `Throwable.getStackTraceDepth()I` and the six `StackStreamFactory` entries are
  the interesting half of the `java/lang` dead list.** They are not deprecated
  API stubs; they are natives written against a JDK-internal shape that JDK 25
  changed. Each is a small piece of evidence that a native was written against a
  specific JDK version and never re-checked. Nobody owns them and this lane does
  not own their files.
