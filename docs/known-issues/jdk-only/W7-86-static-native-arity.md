# W7-86 — natives that index `args` for the wrong receiver shape

Status: the calling convention was **measured** first, then six LIVE rows were
found and five repaired. `Continuation.pin()`/`unpin()` — the residual
W7-75-continuation-forkjoinpool-alias.md §8(2) left — are two of them, and they
are the *smallest* two: the sweep they justified found a `Runtime.exit(int)`
that exits **0 for every status**, on the default configuration.

Branch `fix/continuation-pin-static-arity-20260812`.

> **2026-08-12 (lane B8) — §4.1 row 4 (`Runtime.exit`) is RETIRED ON A
> TRANSCRIPT.** Every pass on this record so far read source and ran nothing;
> the headline claim was that `Runtime.exit(int)` "exits **0** for every
> status, on the default configuration". Run on
> `/c/craton/jdkonly-wave2-target/release/cratonvm.exe --jdk-only` against
> Temurin `jdk-25.0.3.9-hotspot`, all three exit routes, requested status 3 and
> 0, reading the **process** exit code:
>
> ```
> how=runtime requested=3  HotSpot_exit=3  CratonVM_exit=3
> how=runtime requested=0  HotSpot_exit=0  CratonVM_exit=0
> how=system  requested=3  HotSpot_exit=3  CratonVM_exit=3
> how=system  requested=0  HotSpot_exit=0  CratonVM_exit=0
> how=halt    requested=3  HotSpot_exit=3  CratonVM_exit=3
> how=halt    requested=0  HotSpot_exit=0  CratonVM_exit=0
> ```
>
> Status 3 was chosen deliberately over 1: a VM that dies on an unrelated error
> also exits 1, so a `1` row cannot tell a working `exit` from a crash. The
> `0` rows are the negative control — they confirm the 3s are carried through
> rather than a constant. The repair recorded at
> `native-builtins/src/lib.rs:14483` (the `lib.rs` registration re-pointed so
> last-write-wins no longer kills `lang_system.rs:1580`) is therefore live in
> the shipping binary. **Row 4 closed.**
>
> **Scheduling: still none.** No suite vector asserts a non-zero process exit
> status, so this row was fixed and stayed unwitnessed; the transcript above is
> the only evidence, and it is not repeatable by any gate. §4.2's four sampled
> rows (`HexFormat.fromHexDigits`, `fromHexDigitsToLong`,
> `NetworkInterface.getByName`) were **not** exercised by this lane and remain
> open as written. Record **STAYS OPEN** for §4.2.
>
> **RE-VERIFIED 2026-08-12 (lane A1, `--jdk-only` field-updater lane). STAYS
> OPEN, with one row closed and one correction.** Source-level only: this lane
> may not invoke `cargo` either, and it ran nothing. Every claim below says
> which file it was read in.
>
> **The five "FIXED" rows of §4.1 are in the tree.** Each carries its own repair
> marker, so the check is not a line-band guess:
>
> | row | verified at | what is there now |
> |---|---|---|
> | 1,2 `Continuation.pin`/`unpin` | `native-builtins/src/phases_late/concurrent.rs:8580`, `:8592` | `register_with_kind(cls, "pin", "()V", \|ctx, _args\| { ctx.vt_pin(…); Ok(None) }, Bridge)` — no `obj_arg` anywhere in either body, and the comment block above them states the mode analysis |
> | 3 `ClassLoader.findBootstrapClass` | `native-builtins/src/lib.rs:16143` (registration), `:28823` (body, whose comment opens *"W7-86. `findBootstrapClass` is **`private static native`** on JDK 25's …"*) | repaired |
> | 4 `Runtime.exit` | `native-builtins/src/lib.rs:14483` — `registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit)`, with `:14467`–`:14482` carrying the rationale | the `lib.rs` row now points at the instance body, so last-write-wins no longer kills `lang_system.rs:1580` |
> | 5 `Perf.createByteArray` | `native-builtins/src/lang_system.rs:5308` | comment opens *"W7-86. `createByteArray` is … an INSTANCE method"*, and the body reads `args[5]` |
>
> **§4.1 row 6 is CLOSED, and §6 item 1 is therefore stale.**
> `MemorySessionImpl.checkValidState(Ljava/lang/foreign/MemorySegment;)V` was
> repaired by **W7-89**, whose comment at
> `native-builtins/src/phases_late/foreign_ffm.rs:2073` credits this record's
> §4.1 row 6 by name. The body no longer hands the segment to
> `p67_session_check_valid`; it is now
> `let segment = obj_arg(args, 0)?; p67_segment_check_scope(ctx, segment)?;`,
> i.e. resolve the segment's session and check *that* — the JDK's own shape. §6
> item 1's "wants an owner who can build" is answered. §6 items 2–5 are
> untouched.
>
> **§4.2 is still open, and was re-read rather than assumed.** All four sampled
> rows still carry the defect verbatim:
> `HexFormat.fromHexDigits(CharSequence)I` and `fromHexDigitsToLong` at
> `native-builtins/src/phases_late.rs:4186` and `:4199` still read `args.get(1)`
> for the *first* parameter of a static, and
> `NetworkInterface.getByName(String)` at
> `native-builtins/src/phases_late/nio_file.rs:18150` still opens
> `obj_arg(args, 1)?` — the `pin`/`unpin` throwing shape. (Note the line numbers
> have drifted from §4.2's table: `:4181`→`:4186`, `:17936`→`:18150`. Anchor on
> the symbol, not the band.) Nothing here was fixed, and this record correctly
> says so.
>
> **Nothing in §1, §3 or §7 was re-measured.** The calling convention, the sweep
> and the recall figures are transcripts of runs this lane cannot reproduce, and
> are neither endorsed nor challenged here — they are simply unverified by this
> pass. The record therefore stays OPEN on the strength of §4.2, §4.3 and §6
> items 2–5.

**This lane may not invoke `cargo`; the orchestrator builds.** Nothing here was
compiled. Everything measured was measured by *running* — the prebuilt dev
binary `target/release/cratonvm.exe` of 2026-08-12 07:00 (pre-change) against
Eclipse Adoptium 25.0.3.9, on this Windows host — and every claim says which
kind it is. The word "measured" below always means a command was run and its
output is reproduced; "source-level" means it was read.

---

## 1. The calling convention, established from a running VM

W7-75 §8(2) said a lane picking this up "should measure what CratonVM actually
passes for a zero-arg static native before changing anything". That was the
right instruction and it was followed literally, because a lane working the same
family on the same day got it wrong from a source-only reading: its
`Runtime.load0`/`loadLibrary0` diagnosis was one index off, and what caught it
was a *running* error message (an empty name in `no  in java.library.path`), not
another read of the source.

The trap in measuring it is that most JDK methods CratonVM registers a native
for also have real bytecode, so a correct answer proves nothing about the
native. `probes/StaticNativeArityProbe.java` section A therefore uses only
natives the JDK provides **no** bytecode for — the census column
`real_declaring_method` reports `acc_native: true, has_code: false` for each —
so a right answer can only have come from the Rust body:

| line | method | shape | the Rust body reads | measured, both VMs |
|---|---|---|---|---|
| A1 | `System.identityHashCode(Object)` | `public static native` | `args[0]` as the object | `identityHashCode(o) == o.hashCode()` → **true** |
| A2 | `System.identityHashCode(null)` | — | — | 0 (the null control) |
| A3 | `StrictMath.sqrt(double)` | `public static native` | `args[0]` as the double | 4.0 |
| A4 | `Object.getClass()` | instance native | `args[0]` as the receiver | `java.lang.Object` |
| A5 | `Object.wait(long)` | instance native | `args[0]` receiver, `args[1]` millis | returns (a body reading the millis from the wrong slot blocks forever) |
| A6 | `System.arraycopy(…5 params…)` | `public static native` | `args[0..4]` as the five parameters | `1,4` |

A1 is the load-bearing line. If a static native were handed a receiver slot, the
object would be at `args[1]`, `args[0]` would not match `Value::Object`, and the
answer would be 0 — which is exactly what A2 legitimately returns, which is why
A1 compares against the *instance* `hashCode()` of the same object rather than
against a constant.

**Result, measured, identical on HotSpot 25.0.3.9 and CratonVM:**

> `args` carries **exactly the descriptor's parameters**, preceded by a receiver
> **only when the method is an instance method**. A zero-parameter static native
> is called with an **empty** `args`.

That is why `obj_arg(args, 0)?` in a zero-parameter static is not a style
question: `obj_arg` on an out-of-range index returns
`NullPointerException("null object argument")`.

It also means the two mistakes are different shapes and must be searched for
separately:

* **declared static, body written for an instance** — every parameter is read
  one slot too high, so the *last* parameter reads off the end;
* **declared instance, body written for a static** — every parameter is read one
  slot too low, so parameter *k* is read out of the slot holding parameter *k-1*
  (and parameter 0 out of the receiver). Nothing runs off the end, so an
  over-read check cannot see it. `Runtime.exit` is in this half.

## 2. `pin`/`unpin` really are static — and the registration really was live

```
$ javap -p --module java.base jdk.internal.vm.Continuation      # Adoptium 25.0.3.9
  public static native void pin();
  public static native void unpin();
```

Descriptor `()V`, zero parameters, `ACC_STATIC`. A call site is an
`invokestatic` with zero operands. Both registrations opened
`obj_arg(args, 0)?`.

Liveness was **not** taken from a static call-graph walk. It was taken from
`--dump-native-registry` on the same run as the probe, whose `owns_slot` column
is the last-write-wins winner and whose `invocations` column counts dispatches:

```
jdk/internal/vm/Continuation.pin   ()V  owns_slot=true  invocations=1
   registered_by = native-builtins/src/phases_late/concurrent.rs:8534
   real_declaring_method = {loaded:true, declared:true, acc_native:true, has_code:false}
```

`acc_native: true, has_code: false` closes the "maybe real bytecode answered"
door: on the real class there is no bytecode to answer with. `invocations: 1`
says the probe's call reached *this* row.

The measured RED, `probes/StaticNativeArityProbe.expected.txt`:

```
                        HotSpot 25.0.3.9   CratonVM (default, before)
B1 Continuation.pin()   returned:ok        THREW:java.lang.NullPointerException:null object argument
B2 Continuation.unpin() returned:ok        THREW:java.lang.NullPointerException:null object argument
```

**The observable is the exception, not the absence of one.** A native that read
the wrong index and happened not to fault also "does not throw", so the probe
prints the exception *identity* beside HotSpot's rather than asserting
non-throwing.

**W7-75 §8(2) was right about the defect and wrong about one detail:** it says
the on-object pin counter is skipped because `ContSlots::pin` is `None` on the
real class. True, but weaker than the truth — the counter was unreachable in
**both** modes and always has been, because a static has no receiver under
either. It is removed here rather than made conditional. `ContSlots::pin` stays;
`<init>` still uses it, and it is the fallback map's honest description of the
synthetic layout.

`B3` (pin/unpin from inside a mounted `Continuation`) still throws
`ExceptionInInitializerError` after the fix: `Continuation.<clinit>` refuses
with `UnsupportedOperationException: VM does not support continuations`.
CratonVM cannot construct a real `Continuation` at all. That is a separate and
much larger gap; B1/B2 reach the natives without it because a registered native
is dispatched without running the class's `<clinit>`. It is printed anyway so
that "pin/unpin work" is never read as "continuations work".

## 3. The sweep

Method, all three stages mechanical and re-runnable:

1. **Extract** every `register` / `register_with_kind` triple from the native
   crates, resolving `let cls = "…"` and `const` bindings, and capture the
   registered body — the inline closure, or the named `fn` looked up by name.
   13,201 registrations, 10,583 distinct triples, 1,477 distinct classes.
   *(The parser's one non-obvious rule: the comma inside a closure's parameter
   list `|ctx, args|` is not an argument separator. Missing that truncates every
   inline-closure body to `|ctx` and silently drops three quarters of the
   corpus — the first run of this sweep did exactly that and found neither
   `pin` nor `unpin`.)*
2. **Oracle**: a Java program over the 1,007 JDK-package class names, using
   `getDeclaredMethods()`/`getDeclaredConstructors()` and modifier reads only
   (never `setAccessible`, so non-exported `jdk.internal` packages answer),
   emitting `class name descriptor STATIC|INSTANCE NATIVE|JAVA`. 910 classes
   resolved on the JDK 25.0.3.9 image, 23,753 member rows; 97 classes are
   third-party and absent, and are excluded rather than guessed at.
3. **Two detectors**, one per shape of §1:
   * *over-read*: the highest `args` index the body reads is `>=` the expected
     length (`params + (0 if static else 1)`);
   * *shape vote*: for every typed read `(index, Value tag)`, score the
     hypothesis "`args[i]` is parameter *i*" against "`args[0]` is the receiver
     and `args[i]` is parameter *i-1*", and flag any registration whose
     best-fitting shape contradicts the image's `ACC_STATIC` bit.
4. **Liveness** from `--dump-native-registry`, never from a call-graph walk. A
   static walk was tried and is recorded here as a **failed instrument**: `fn
   register` is a name collision across a dozen crates, so unioning their bodies
   makes every registrar reachable from every root and the answer comes back
   "everything is live in both modes", which is worthless. Two of its earlier
   spellings gave two *different* wrong answers before that was noticed. The
   census answers the same question by measurement.

The over-read detector alone produced 63 hits and is mostly noise: a handler
registered for several overloads legitimately probes indices the narrowest
overload does not have (`Class.getDeclaredFields`/`getDeclaredFields0(Z)`,
`LockSupport.park()`/`park(Object)`, `Object.wait()`/`wait(J)`,
`Unsafe.putLong`/`putLongUnaligned`, the erased `([Ljava/lang/Object;)`
`VarHandle` signature-polymorphic descriptors). Every one of those was read and
dismissed. The shape voter is what found the second half of the population,
including `Runtime.exit`.

## 4. The population

Liveness column is measured on a **default** run (`--real-jdk` is the default,
no flag passed): `LIVE-OWNER` = that exact site owns its slot;
`ABSENT` = the triple has no row at all; `ABSENT-SITE` = the triple is served,
but by a different, correctly-indexed registrar and this site never registered.

### 4.1 LIVE in Compatible mode — six rows

| # | triple | real shape | body assumed | symptom | disposition |
|---|---|---|---|---|---|
| 1 | `jdk/internal/vm/Continuation.pin()V` | static, 0 params | instance | NPE where HotSpot returns | **FIXED** |
| 2 | `jdk/internal/vm/Continuation.unpin()V` | static, 0 params | instance | NPE where HotSpot returns | **FIXED** |
| 3 | `java/lang/ClassLoader.findBootstrapClass(Ljava/lang/String;)Ljava/lang/Class;` | `private static native` | instance (`args[1]`) | **null for every name, always** | **FIXED**, own commit |
| 4 | `java/lang/Runtime.exit(I)V` | `public void exit(int)` | static (`args[0]`) | **exits 0 for every status** | **FIXED** |
| 5 | `jdk/internal/perf/Perf.createByteArray(Ljava/lang/String;II[BI)…` | `public native`, instance | static (`args[4]`) | zero-length counter buffer | **FIXED** |
| 6 | `jdk/internal/foreign/MemorySessionImpl.checkValidState(Ljava/lang/foreign/MemorySegment;)V` | `public static` | instance | validity check is a silent no-op | **LEFT**, §6 |

Row 4 is the one worth stopping on. `javap -p` gives `public void exit(int)` —
an instance method — so `args[0]` is the `Runtime` receiver and `args[1]` the
status. `native-builtins/src/lib.rs` pointed the triple at
`native_system_exit`, the body for the **static** `System.exit(int)` whose
`args[0]` *is* the status. Measured, `probes/RuntimeExitArityProbe.java`, exit
codes:

```
arm      HotSpot 25.0.3.9   CratonVM before
runtime  7                  0        Runtime.getRuntime().exit(7)
system   7                  7        System.exit(7)          — the control
zero     0                  0        Runtime.getRuntime().exit(0)
```

and the repair is not new code: `native_runtime_exit` in `lang_system.rs`
already handles the instance shape, and `lang_system.rs:1423` already registers
it for this exact triple. The `lib.rs` line ran **later**, and last-write-wins
made the correct implementation dead. The census shows both rows, `owns_slot`
on the `lib.rs` one. Three arms rather than two because a change that ignored
the argument entirely would also pass a two-arm test.

### 4.2 Not present in a default run — eleven rows

Same defect, in registrars the default (`--real-jdk`) path never reaches. Each
is `ABSENT`/`ABSENT-SITE` in the census, which is a measurement of this build
and configuration, not a claim about the `synthetic-jdk` feature build (which
this binary does not have compiled in).

| triple | site | reads | census |
|---|---|---|---|
| `java/util/HexFormat.fromHexDigits(Ljava/lang/CharSequence;)I` | `phases_late.rs:4181` | `args[1]` | ABSENT |
| `java/util/HexFormat.fromHexDigitsToLong(Ljava/lang/CharSequence;)J` | `phases_late.rs:4194` | `args[1]` | ABSENT |
| `java/net/NetworkInterface.getByName(Ljava/lang/String;)…` | `phases_late/nio_file.rs:17936` | `obj_arg(args,1)?` | ABSENT |
| `java/net/NetworkInterface.getByInetAddress(Ljava/net/InetAddress;)…` | `phases_late/nio_file.rs:17955` | `obj_arg(args,1)?` | ABSENT |
| `java/security/Security.{getProvider,addProvider,insertProviderAt,removeProvider,getProperty,setProperty,getAlgorithms}` | `phases_early.rs:16681…16845` (7 rows) | `args[1]`/`args[2]` | ABSENT-SITE — served by `jca/provider_chain.rs`, which reads `args[0]` |

`native-builtins/src/net_phase_e.rs:15119` already says in a comment that the
`nio_file.rs` twin "is reachable only through `register_synthetic_overrides`,
i.e. `--synthetic-jdk`" — the census agrees with that comment, which is a
useful cross-check on the instrument.

Two of these would **throw**, not merely answer wrongly: `getByName` and
`getByInetAddress` use `obj_arg(args, 1)?` on a one-element `args`, which is the
`pin`/`unpin` failure mode exactly.

### 4.3 Superseded — five rows, dead code, deliberately not fixed

| triple | dead site | slot owner |
|---|---|---|
| `java/lang/Integer.toHexString(I)Ljava/lang/String;` | `phases_early.rs:2494` | `lang_math.rs:282` |
| `java/lang/Integer.toBinaryString(I)…` | `phases_early.rs:2504` | `lib.rs:11315` |
| `java/lang/Integer.toOctalString(I)…` | `phases_early.rs:2514` | `lib.rs:11301` |
| `java/lang/Long.toHexString(J)…` | `phases_early.rs:2677` | `lib.rs:11379` |
| `java/util/Collections.frequency(Ljava/util/Collection;Ljava/lang/Object;)I` | `phases_late/collections.rs:186` | `native-collections/src/lib.rs:50820` |

Each of these reads `args[0]` as a `Value::Object` where the static's first
parameter is an `int`/`long`, or `args[1]`/`args[2]` for the two parameters of a
static — the §1 instance shape. Repairing them changes nothing that can be
dispatched, which is the inert-fix shape eight lanes shipped on 2026-08-12.
They are reported, not touched.

Independently confirmed by running: `Collections.frequency(list,"a")` answers 2
and `Security.getProperty("keystore.type")` answers non-null on CratonVM today,
which the mis-indexed bodies could not do.

## 5. Blast radius

Four commits, each independently revertible.

* **`pin`/`unpin`** (`phases_late/concurrent.rs`). The whole surface is two
  zero-parameter voids that today throw NPE. Nothing can depend on that NPE
  except a caller catching `Throwable`, and the JDK's own callers
  (`Continuation.enter`/`onPinned` paths) are unreachable anyway because
  `Continuation.<clinit>` refuses. `vt_pin`/`vt_unpin`, the observable half, are
  byte-for-byte unchanged. Synthetic mode: the removed on-object counter could
  never be written there either (§2), so no synthetic behaviour changes.
  **Smallest radius of the four.**
* **`Runtime.exit`** (`lib.rs`). Changes the exit code of
  `Runtime.getRuntime().exit(n)` from 0 to n. Anything that *depended* on the 0
  was depending on a bug; but note that a test harness scoring a forked JVM by
  its exit status will now see failures it previously scored as passes. That is
  the correct direction and it is worth expecting rather than being surprised
  by. `System.exit` is untouched — the same `native_system_exit` still serves
  it, and the `system` arm of the probe is the control that pins that.
* **`ClassLoader.findBootstrapClass`** (`lib.rs`). **The largest radius, and the
  one to revert first if the suite reddens** — it is committed alone for that
  reason. It has answered null for every name since it was written, so this
  wakes a path that has been inert:
  `ClassLoader.findBootstrapClassOrNull` sits on the parent-delegation path of
  every `loadClass`. The body's existing guards are unchanged (a generated proxy
  name, and any class with a recorded defining loader, still answer null), and
  the probe's C2 arm is the control that a name with no bootstrap class still
  answers null.
* **`Perf.createByteArray`** (`lang_system.rs`). A direct `ByteBuffer` of the
  requested size instead of 0 bytes, on a class exported to nobody.

**No `NativeKind` changed.** Every edit is either a callback pointer or an index
inside a body; no `set_category`/`with_category` scope was moved, opened or
closed, and no `register` became a `register_with_kind` or vice versa. In
particular nothing here is inside the `ForkJoinPool` ambient-`Bridge` scope the
real-JDK keep-list in `native-api/src/registry.rs` depends on.

**No `CRATONVM_*` flag was added**, so `types/src/flag_groups.rs`,
`types/tests/flag-surface.txt`, `docs/flag-tokens.md` and
`docs/config/flag-inventory.md` are untouched.

**No test was weakened.** Nothing existing was edited; two probes were added.

## 6. What was not fixed, and why

1. ~~**`MemorySessionImpl.checkValidState(MemorySegment)`** (§4.1 row 6) is LIVE
   and is not repairable by moving an index. The method is static, so `args[0]`
   *is* the right slot — but it holds the `MemorySegment` being accessed, and
   the body passes it to `p67_session_check_valid` as if it were the session.
   `p67_session_modelled` then gates on the object having the session's field
   count and an `Int` at the state slot, which a segment does not, so the
   function returns `Ok(())`: **the check fails open and validates nothing.**
   A correct repair needs the segment→session mapping, which is a different
   piece of the FFM model and wants an owner who can build. Failing open is the
   safe direction meanwhile, which is why this is filed rather than guessed at.~~
   **CLOSED by W7-89** — verified 2026-08-12 at
   `native-builtins/src/phases_late/foreign_ffm.rs:2073`, whose comment credits
   this section by name. The segment→session mapping the paragraph above said
   was missing already existed as `p67_segment_check_scope`, and the body now
   calls it. See the status block at the top of this file.
2. **`Perf.createByteArray` has no probe** and none is pretended.
   `jdk.internal.perf` is exported to nobody and `Perf.getPerf()` is gated, so
   the defect is not observable from ordinary Java. A Rust unit test on the
   argument decode is the right instrument and is deliberately **not** added
   here: this lane cannot build, and an unverifiable test in `native-builtins`
   risks a compile failure, which in this tree is a broken build rather than a
   broken test.
3. **The eleven rows of §4.2 and the five of §4.3.** §4.3 cannot be dispatched;
   §4.2 needs a `--synthetic-jdk` build to measure, and repairing on a
   source-only reading is precisely what this campaign keeps paying for.
4. **`StackStreamFactory$AbstractStackWalker.fetchStackFrames(IJIII[…)I`** trips
   both detectors and neither verdict is trustworthy: the body reads eight
   indices with mixed tags across two registered descriptors. It wants reading
   against the JDK's own `fetchStackFrames` contract, not against an arity
   heuristic. Untouched.
5. **`Continuation.<clinit>` refusing** (§2, probe line B3). Constructing a real
   `Continuation` fails on `ContinuationSupport.ensureSupported`. Out of scope
   here, and the probe prints it so it cannot be mistaken for success.

## 7. Recall — what this sweep could still be missing

Stated so the next reader knows the shape of the gap rather than inheriting a
number.

* **1,100 call sites (7.7% of the 14,301 seen)** could not have their triple resolved
  statically — the class/method/descriptor is a computed expression rather than
  a literal or a simple binding (`&format!("(L{XERCES_XML_STRING};)I")` and
  friends). They were skipped, not guessed.
* **148 registrations** name a handler `fn` the body index could not find.
* **3,450 registrations** name a class or member the JDK 25 image does not
  declare (third-party classes, CratonVM-internal carriers, and members that
  simply are not there). Those have no `ACC_STATIC` bit to compare against, so
  no verdict is possible — and a fabricated one would be worse.
* The shape voter only sees reads it can type. A body that pulls `args[i]`
  into a variable and matches on it later contributes nothing to either score.

The corpus that *was* adjudicated is **9,751 registrations** against the real
image, and it is that number the §4 population is complete with respect to.

## 8. Reproducing

```
# the oracle and the RED, side by side
javac --add-exports java.base/jdk.internal.vm=ALL-UNNAMED -d out \
      probes/StaticNativeArityProbe.java probes/RuntimeExitArityProbe.java
java     --add-exports java.base/jdk.internal.vm=ALL-UNNAMED \
         --add-opens    java.base/java.lang=ALL-UNNAMED -cp out StaticNativeArityProbe
cratonvm --add-exports java.base/jdk.internal.vm=ALL-UNNAMED \
         --add-opens    java.base/java.lang=ALL-UNNAMED -cp out StaticNativeArityProbe

for arm in runtime system zero; do java -cp out RuntimeExitArityProbe $arm; echo "$arm -> $?"; done

# liveness, last-write-wins winners and dispatch counts
cratonvm --dump-native-registry native-census.json --explain-jdk-only -cp out StaticNativeArityProbe
```

Both spellings of `--add-exports` (space-separated and `=`) work on CratonVM;
the `=` spelling is used above and in the probe's own javadoc.
