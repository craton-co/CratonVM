# W7-93 — `StackWalker$Option`'s constants: one null, three nameless, and a native `<clinit>` that froze the JDK-21 shape

**Status: SOURCE LANDED 2026-08-12, NOT BUILT and NOT RUN.** The measurements in
§1–§3 are real runs of the pristine `dev` control binary (`44044c7e2`) and of
HotSpot JDK 25 on this host, taken by this lane. Everything about the *fix* in
§5 is source-only: this lane was not permitted to build, so no claim that the
repaired native produces the numbers §6 asserts has been observed. The fixture
half **is** verified in both directions — it passes on HotSpot and fails on the
control at the intended assertion (§6).

**This file is the enum-identity record, and it now holds TWO causes.** §1–§7 are
`StackWalker$Option`: a native `<clinit>` fabricated the constants themselves.
**§8 is a second, different defect on a different class** — `Thread$State`'s
constants are correct, but natives on its `values()`/`valueOf()` mint a fresh
instance per call, so `values()[0] != Thread.State.NEW`. §8 was measured on the
binary that already carries §5's fix, so the two are independent; §1's
"`Thread$State` ok" was a non-null check, not an identity check, and §8.1 says
so.

---

## 1. Answer first: this is StackWalker-only, not a real-JDK-enum problem

This is the single most valuable thing to establish and it is cheap, so it goes
first. Same probe, same binary, same run:

```text
CONTROL 44044c7e2, --jdk-only
  java.lang.StackWalker$Option      values()=3   constants: 1 null, 3 nameless   BROKEN
  java.time.DayOfWeek               values()=7   MONDAY..SUNDAY, names+ordinals   ok
  java.nio.file.StandardOpenOption  values()=10  READ..DSYNC, names+ordinals      ok
  java.lang.annotation.RetentionPolicy  RUNTIME  ok
  java.util.concurrent.TimeUnit         SECONDS  ok
  java.lang.Thread$State                NEW      ok
```

Every other real JDK enum measured initialises correctly through its **own real
`<clinit>`**, with the right count, the right names and the right ordinals. The
general machinery — `Enum.<init>(String,int)`, `putstatic`, the javac-generated
`$values()`, `Class.getEnumConstantsShared`, `EnumSet`, `Enum.valueOf` — is
sound in this VM. Nothing about enums is broken.

`StackWalker$Option` is broken because it is the one enum whose `<clinit>` a
CratonVM native replaces.

> **§1's scope claim was measured with a probe too shallow to see the second
> cause, and is now known to be wrong for `Thread$State`.** The probe above
> asked only whether each constant was non-null and correctly named — which
> `Thread$State`'s constants are. It never asked whether `values()` returns
> *those* objects. It does not: a different native, on a different method,
> mints a fresh instance per call. See §8. The narrower claim §1 was really
> testing does survive: no enum's own `<clinit>` is broken, and the general
> machinery is sound. What §1 could not see is that publishing the constants
> correctly is not sufficient when a native also owns the *accessors*.

## 2. What actually reads back — the reporting premise is half right

The premise handed to this lane was *"every `StackWalker$Option` constant reads
back null"*. Measured, that is true of exactly **one** of the four:

```text
CONTROL 44044c7e2, --jdk-only              HOTSPOT jdk-25.0.3.9
RETAIN_CLASS_REFERENCE  non-null           non-null
    name()  = null                             name() = "RETAIN_CLASS_REFERENCE"
    ordinal = 0                                ordinal = 0
SHOW_HIDDEN_FRAMES      non-null           non-null
    name()  = null                             name() = "SHOW_HIDDEN_FRAMES"
    ordinal = 0    <-- also 0                  ordinal = 3
SHOW_REFLECT_FRAMES     non-null           non-null
    name()  = null                             name() = "SHOW_REFLECT_FRAMES"
    ordinal = 0    <-- also 0                  ordinal = 2
DROP_METHOD_INFO        NULL               non-null, ordinal 1
values().length         3                  4
valueOf("RETAIN_CLASS_REFERENCE")          returns the constant
    -> IllegalArgumentException: No enum constant …
Set.of(DROP, RETAIN)                       size 2
    -> NPE: Cannot invoke "Object.equals(Object)" because "e0" is null
```

**The correction matters, because the nameless half is the harder defect.** A
null constant fails loudly at the first dereference. A non-null constant with
`name() == null` and `ordinal() == 0` passes every null-check in the JDK and
then silently mis-answers: `Enum.valueOf` cannot find any of them (the constant
directory is keyed by `name()`), `compareTo` reports every pair equal, and an
`EnumSet` over them collapses to one bit because all three share ordinal 0.
Anything that switches on an `Option` takes the wrong arm rather than throwing.

The wild victim is the null one. `javax.crypto.JceSecurityManager.<clinit>` ends
with `StackWalker.getInstance(Set.of(Option.DROP_METHOD_INFO, Option.RETAIN_CLASS_REFERENCE))`;
`e0` is `DROP_METHOD_INFO`, `ImmutableCollections$Set12.<init>` NPEs on it, the
class dies as `ExceptionInInitializerError`, and `Cipher.getMaxAllowedKeyLength`
→ `getConfiguredPermission` → `getstatic JceSecurityManager.INSTANCE` takes the
whole real `KeyGenerator.getInstance` path down with it. Reproduced on the
control:

```text
maxKeyLen(AES)  HotSpot: 2147483647   CratonVM: ExceptionInInitializerError
```

That is a *consequence*, not the defect. Anything reaching a real
`StackWalker.getInstance(Set)` with `DROP_METHOD_INFO` in it hits the same thing,
and everything reaching an `Option` at all gets the nameless constants.

## 3. Mechanism — established, not assumed

**The real class bytes are loaded.** This was the first thing to rule out and it
is refuted outright. Under `--jdk-only`, `java.lang.StackWalker$Option` reflects
as the genuine JDK 25 class:

```text
declaredFields n=5
  RETAIN_CLASS_REFERENCE, DROP_METHOD_INFO, SHOW_REFLECT_FRAMES,
  SHOW_HIDDEN_FRAMES, private static final [LStackWalker$Option; $VALUES
declaredMethods n=3
  values(), valueOf(String), private static $values()
isEnum=true  super=java.lang.Enum
```

Four constants, `$VALUES`, and the three javac-generated bodies. This is not a
fabricated stand-in — the fabricated shape (`classloading/src/class_manager.rs`,
the `"java/lang/StackWalker$Option"` arm of the synthetic field table) declares
**three** static fields and no `$VALUES`. So the real `<clinit>` exists, is
loaded, and would have worked. `javap -p -c` confirms it is the ordinary four ×
(`new` / `ldc <name>` / `iconst_<ordinal>` / `invokespecial <init>(String,int)` /
`putstatic`) followed by `invokestatic $values()` — nothing exotic, nothing this
VM cannot run, and nothing it does not already run for `DayOfWeek`.

**It does not run, because a native is registered for its triple.**
`native-builtins/src/stack_walker.rs::register_stack_walker_boot` registers
`("java/lang/StackWalker$Option", "<clinit>", "()V")` → `native_option_clinit`.
Per docs/architecture/natives-over-real-jdk-classes.md §1, **registration itself
is the gate**: a registered native beats real bytecode unconditionally on the
cold interpreter paths, with no list consulted and `NativeKind` only ever
subtracting. The registrar ships in **both** modes —
`vm/src/vm/vm_init.rs` calls `register_essential_natives_with_shims` directly on
the real-JDK arm, and the synthetic arm reaches the same function through
`register_builtins` → `register_essential_natives`. So the shadowing is not a
synthetic-mode artefact; it is unconditional.

**What the native did.** It allocated one bare instance per name from a
hard-coded three-name list, stored each straight to its static field, and built a
three-element `$VALUES`. It never ran `Enum.<init>(String,int)` — hence
`name() == null` and `ordinal() == 0` everywhere — and its list predates JDK 22,
hence `DROP_METHOD_INFO` never written and left null. Both symptoms fall straight
out of those two lines.

**The registrations in `native-builtins/src/phases_late.rs` are dead**, and this
was measured rather than reasoned. Three static-field natives are registered
there for `RETAIN_CLASS_REFERENCE` / `SHOW_HIDDEN_FRAMES` / `SHOW_REFLECT_FRAMES`
with a FIELD descriptor in the method registry's descriptor slot. A `getstatic`
resolves through the class's static-field storage and never consults the native
method registry, and nothing invokes a *method* by those names — so the triple
they key is one no dispatch can produce. The proof is identity: the objects a
program reads out of those statics are the ones `native_option_clinit`
allocated, and `values()[0] == Option.RETAIN_CLASS_REFERENCE` holds. Had the
`phases_late` natives run, they would have handed back fresh `p57_alloc_enum`
instances that are **not** `==` to the `$VALUES` entries. They are inert today
and would be actively harmful if a dispatch change ever made them live; a
tombstone comment now says so in place.

### 3.1 The premise the shadowing was built on, and why it was never re-checked

`stack_walker.rs`'s module banner justifies the native `<clinit>` with *"that
clinit in turn depends on MethodHandles.Lookup's clinit, which is fragile in our
VM bootstrap order."* That claim is undated, unverified, and is the only stated
reason this module shadows a real initialiser. §1 is the evidence against it: six
real JDK enums, five of them fine. **A guard scoped by a stated premise is only
as good as the premise** — the banner now carries the measurement next to the
claim.

### 3.2 How a green record froze the divergence

stackwalker-option-enum-constants-null-blocks-es-suite-FIXED.md
closed this area on 2026-07-10 by adding `$VALUES` to the same native, and its
Verification section records, as evidence of success:

> `StackWalker.Option.values()` returns length `3`; `Class.getEnumConstants()`
> returns length `3`.

Three is the wrong answer on any JDK ≥ 22. The unit test
`option_clinit_populates_enum_values_array` asserted `array_length == 3` and
identity between statics and `$VALUES` — both true of the broken shape, because
identity holds fine between three wrong constants and a three-long array of the
same three wrong constants. Neither the record nor the test ever asked what the
*class* said its constants were. This is the freeze-the-divergence species: a
test that pins today's output locks the divergence in.

## 4. Blast radius

`--jdk-only` and `--real-jdk` alike (the registrar is unconditional), on any JDK
≥ 22 for the null half and on every JDK for the nameless half. Reached by
anything that touches `StackWalker.Option`: the real JCA path above, and — per
`stack_walker.rs`'s own banner — Keycloak / WildFly / JBoss-Modules boot
detectors and Lucene's `TestSecrets.ensureCaller`. Those detectors survive only
because they accept any non-null walker and never read a constant's name.

## 5. What landed, and why this fix rather than the other two

`native_option_clinit` now derives its constant list from the class's **own
declared static fields** — every `static` field whose descriptor is the enum's
own type, in declaration order, which is ordinal order; `$VALUES` filters itself
out because its descriptor is the array type. Each constant is then given the
`name` and `ordinal` a real `<clinit>` would have passed to
`Enum.<init>(String,int)`, written to the two slots
`lang_misc::native_enum_init` writes and resolved against `java/lang/Enum` (never
the receiver's class, for the reason `native_enum_name` records). `$VALUES` is
built by re-reading each published static, so `values()[i] == <the constant>`
holds by construction. The historical three-name list survives only as the
fallback for a receiver whose declared fields cannot be read at all.

Version-proofing is the point of deriving rather than hard-coding: JDK 21 gives
three, JDK 25 gives four, a future JDK giving five costs nothing. **Adding
`DROP_METHOD_INFO` as a fourth hard-coded name would have fixed today's JDK and
re-armed the same trap for the next one**, which is what the brief asked to
avoid.

Two rejected alternatives, both of which are more correct in principle:

* **Delete the registration and let the real `<clinit>` run.** This is the right
  end state and §1/§3 are the argument for it. It is not done here because the
  registrar is shared: in `--synthetic-jdk` the class is FABRICATED with three
  static fields and no `<clinit>` bytecode, and `class_manager.rs` additionally
  injects an `ACC_NATIVE` `<clinit>` into that fabricated shape — so deleting the
  registration alone converts synthetic mode from wrong constants to
  `UnsatisfiedLinkError`. Doing it properly means making the registration
  conditional on the **runtime** JDK mode (not a `cfg`, which cannot see it), and
  that is a change worth building and running before landing. It is left open in
  §7.
* **Fix the `phases_late.rs` static-field natives.** They cannot fire at all
  (§3), and if they could they would break `==` identity against `$VALUES`.

What landed is therefore explicitly a **repair of the fabrication, not an
endorsement of it**. The native still fabricates where the real `<clinit>` could
have run; it now fabricates the same thing the real `<clinit>` would have
produced, and the banner and §7 name the removal as the remaining work.

## 6. Coverage

`regression-suite/src/RJdkStrict.java` (in `JDKONLY_CLASSES`, so it is
scheduled) gains `realEnumsAreSelfConsistent()`. It asserts, for
`StackWalker$Option` **and five unrelated real JDK enums**, that
`values()`, `getEnumConstants()`, `valueOf()`, `name()`, `ordinal()`,
`Set.of(values())` and `EnumSet.allOf()` all agree with what the class's own
declared static fields say — plus `StackWalker.getInstance(Set)`, the call the
JCA path makes.

Not one check names a constant or a count. The invariant is self-consistency
against the loaded class, so it holds on any JDK and cannot rot the way the
three-name list did. The five sibling enums are there so the fixture also pins
the §1 generalisation: if this ever stops being StackWalker-only, the fixture
says which enum.

Verified in both directions before landing:

```text
HotSpot jdk-25.0.3.9          PASS RJdkStrict (347 checks)   CK enumSelfConsistent=6
HotSpot, javac --release 17   PASS RJdkStrict (347 checks)   (run.sh RELEASES="17 21 25")
CONTROL 44044c7e2 --jdk-only  AssertionError at the intended check:
    "java.lang.StackWalker$Option.values() has 3 entries but the class
     declares 4 constants [RETAIN_CLASS_REFERENCE, DROP_METHOD_INFO,
     SHOW_REFLECT_FRAMES, SHOW_HIDDEN_FRAMES]"
```

The unit test `option_clinit_populates_enum_values_array` now also asserts each
constant's `name` and `ordinal`, so the shape that read green through this whole
defect no longer does.

## 7. Left open

1. **The removal.** Make the `Option.<clinit>` registration conditional on the
   runtime JDK mode so the real initialiser runs whenever real class bytes are
   authoritative, and drop the `ACC_NATIVE` `<clinit>` injection from
   `class_manager.rs`'s fabricated shape at the same time. Needs a build and a
   synthetic-mode run; both halves must move together.
2. **The dead `phases_late.rs` registrations should be deleted**, not just
   commented. Deleting registrations moves
   `scripts/baselines/jdk-only-bridge-ratchet.json`, which needs a build to
   re-freeze, so it is out of scope for a no-build lane.
3. **Nothing here is built or run.** §5's fix is source-only. The first lane with
   a binary should re-run `RJdkStrict` under `--jdk-only` and confirm 347 checks
   and `CK RJdkStrict enumSelfConsistent=6`, and re-run
   `Cipher.getMaxAllowedKeyLength("AES")`, which should stop throwing.
4. **The `--jdk-only` question this record does not answer.** A native that
   fabricates enum constants over a real, loaded, perfectly runnable JDK class is
   a strict-mode violation whether or not the constants are right. It is not
   counted as one today. Whether `--jdk-only` should refuse this registration
   outright belongs with the wave-1 enforcement surface, not with this record.
5. The ES record in §3.2 still states `values()` length 3 as verified-good. It is
   an internal fixed-bug doc and out of this lane's files; its Verification
   section needs the correction.

---

# 8. SECOND CAUSE, SAME FAMILY — `Thread$State.values()` mints a fresh instance per call

**Status: MEASURED RED 2026-08-12 on `cratonvm-final.exe` (this wave's frozen
binary, which already carries §5's `Option` fix), FIXED IN SOURCE, NOT BUILT and
NOT RUN.** This section is a *different defect with a different mechanism on a
different class*. It shares only the species — a native standing in front of a
real JDK enum — with §1–§7. Do not read §5's fix as covering it, and do not read
this fix as covering anything on `Option`.

## 8.1 Answer first: the constants are RIGHT; the accessors are wrong

`Option`'s defect was that the constants themselves were fabricated (a native
`<clinit>`). `Thread$State`'s is the exact opposite: `<clinit>` runs, the static
fields hold correct constants, and `getEnumConstants()` agrees with them — but
`values()` and `valueOf()` are *separately* registered natives that allocate a
NEW instance per call and never consult the statics. Measured, same binary, same
run, against HotSpot jdk-25.0.3.9:

```text
                                     CRATONVM --jdk-only     HOTSPOT
Thread.State.NEW == Thread.State.NEW  true                    true    <- statics fine
getEnumConstants()[0] == State.NEW    true                    true    <- $VALUES fine
Thread.currentThread().getState()
                    == State.RUNNABLE true                    true    <- getState fine
values()[0] identity                  @2, then @3 on a        stable
                                      second call
values()[0] == State.NEW              FALSE                   true    <- THE DEFECT
valueOf("NEW") == State.NEW           FALSE                   true    <- THE DEFECT
Arrays.asList(values())
        .contains(getState())         FALSE                   true
```

Three consequences worth naming, because two of them are counter-intuitive:

* `getState()` is **not** the culprit and never was. `lang_system.rs`'s
  `native_thread_get_state` already resolves the constant through
  `static_field_index_by_name` + `get_static_field`, so it hands back the
  canonical object. The prompt's leading hypothesis is refuted by measurement.
* An enum **`switch` still selects the right arm** over a minted constant —
  measured, both `switch(values()[0])` and `switch(valueOf("TERMINATED"))` are
  correct on the red binary. javac's `$SwitchMap` is indexed by `ordinal()`, and
  the minted instances carry correct ordinals. `switch` is therefore *not* a
  detector for this; only `==` shapes are.
* `EnumSet`/`EnumMap` likewise survived in the probe, for the same reason. The
  damage is confined to `==` against a constant, which is exactly what
  `getState() == State.RUNNABLE`, `Arrays.asList(values()).contains(state)` and
  every thread-state monitor and leak detector are written as.

## 8.2 Mechanism — the registration, and why it ships under `--jdk-only`

`native-builtins/src/phases_late/concurrent.rs::register_p71_thread_extras`
registers two natives on `java/lang/Thread$State`:

* `valueOf(Ljava/lang/String;)Ljava/lang/Thread$State;` — maps the name to a
  hard-coded ordinal and calls `p57_alloc_enum`, which **allocates**.
* `values()[Ljava/lang/Thread$State;` — allocates a 6-long reference array and
  fills it with six freshly allocated instances from a hard-coded name list.

Both were correct for `--synthetic-jdk`, where `Thread$State` is a fabricated
class with no `<clinit>` and no static fields to read. They reach `--jdk-only`
because `register_p71_thread_extras` is **also** called from
`register_essential_natives` (`native-builtins/src/lib.rs:9159`), for an
unrelated reason stated at the call site: `ThreadGroup` is needed early for
JBoss-Modules' `JBossThreadFactory` during process-controller bootstrap. The
`ThreadGroup` half of that registrar is what essentials wanted; the
`Thread$State` half came along with it. Registration is the gate, so the real
`values()`/`valueOf()` bytecode never runs.

This is the same rule §3 established for `Option`, applied at a different level:
there, the shadowed method was the initialiser; here, the initialiser is left
alone and the *readers* are shadowed. That is why §1's probe read `Thread$State`
as "ok" — it checked what `<clinit>` produced, which is correct.

Confirmed scope, by identity measurement not inference. Twenty real JDK enums
were checked for full identity (`values()[i] == the declared static field`,
`getEnumConstants()[i] ==` it, and `valueOf(name) ==` it) on the red binary:

```text
OK   DayOfWeek, StandardOpenOption, RetentionPolicy, TimeUnit,
     StackWalker$Option (so §5's area stays green), RoundingMode, Month,
     Locale$Category, ChronoUnit, ChronoField, ProcessBuilder$Redirect$Type,
     LinkOption, StandardCopyOption, ElementType, TextStyle,
     SSLEngineResult$Status, FormatStyle
BAD  Thread$State   all six constants fail values[] and valueOf
```

`Thread$State` is the only one, and the registrar census says why: it is the
only real-JDK enum whose `values`/`valueOf` are registered from a registrar that
essentials calls. Every other `p57_alloc_enum` minting site
(`System$Logger$Level`, `HttpClient$Version`/`$Redirect`, `Normalizer$Form`,
`FormatStyle`, `NumberFormat$Style`, `FileVisitResult`,
`StructuredTaskScope$Subtask$State`, and `phases_late.rs`'s three dead
`Option` static-field triples) sits in a phase registrar that `--jdk-only`
does not run — which the twenty-enum sweep independently confirms rather than
merely asserts.

## 8.3 The fix, and why it is not fabrication

Two helpers land in `native-builtins/src/lang_system.rs`, next to
`native_thread_get_state`, whose canonical-constant shape they generalise:

* `canonical_enum_constant(ctx, class_name, name)` — the object the class's own
  static field holds, or `None`.
* `canonical_enum_values(ctx, class_name)` — a FRESH array (as
  `$VALUES.clone()` returns) holding those same objects in ordinal order.

Both **ask the class**: the constant names come from its declared static fields
filtered by the self descriptor, so declaration order is ordinal order and
`$VALUES` filters itself out by having the array descriptor. That is the same
derivation §5 used for `Option`, and it is why neither helper carries a name
list that a future JDK could invalidate.

Neither helper can fabricate. `canonical_enum_values` returns `None` unless the
class declares at least one constant **and every one reads back non-null**, so a
fabricated synthetic-JDK stand-in (no static fields) and a half-initialised
class both fall through to the caller's existing behaviour rather than yielding
an array with null holes — the shape `ImmutableCollections$Set12.<init>` NPEs
on, per §5. `ensure_class_initialized` returning `Ok` is not treated as proof a
real class answered, because it fabricates rather than failing; the static-field
lookup is the discriminator.

The two `concurrent.rs` natives then try the canonical route first and keep
their existing minting body as the fallback. One code path serves both modes: a
`cfg` feature guard cannot see the runtime JDK mode, and no new flag is
introduced.

**Why not simply delete the registrations.** That is the right end state and is
the same open item §7.1 records for `Option`: under `--synthetic-jdk` the class
is fabricated with no bytecode, so removal alone converts that mode to an
`UnsatisfiedLinkError`. It needs the registration made conditional on the
runtime mode, plus a synthetic-mode run. Out of scope for a no-build lane.

## 8.4 Coverage

`realEnumsAreSelfConsistent()` in `regression-suite/src/RJdkStrict.java` already
carried the failing assertion — `Thread.State` is in its class list and
`values[i] == c` is exactly the check that fired. It is unweakened. Three
shapes are added after the loop, each stating what it can and cannot see:

1. `values()` returns a fresh array whose ELEMENTS are stable across calls.
   This is the direct detector, and it is what a defensive-copy `values()` means.
2. `getState() == State.RUNNABLE`, `Arrays.asList(values()).contains(getState())`
   and `valueOf(getState().name()) == getState()`. The first is a regression
   guard on the already-correct `native_thread_get_state`; the other two fail on
   the red binary.
3. A real enum `switch`, annotated in the source with the §8.1 measurement that
   it does **not** detect this defect, so no later reader mistakes it for one.

Verified on HotSpot jdk-25.0.3.9 before landing:

```text
PASS RJdkStrict (359 checks)   CK RJdkStrict enumSelfConsistent=6
```

347 → 359 checks, `-Xlint:all` clean. The added shapes were each measured
separately on the frozen red binary (§8.1's table) so the record states which of
them actually catch the defect rather than assuming all three do.

## 8.5 Left open

1. **Not built, not run.** The first lane with a binary should re-run
   `RJdkStrict` under `--jdk-only` and confirm 359 checks and
   `CK RJdkStrict enumSelfConsistent=6`.
2. **`Thread$State.valueOf` still does not throw.** The real
   `Enum.valueOf` throws `IllegalArgumentException` for an unknown name; the
   native answers `RUNNABLE`. The fix does not change that — a real name now
   resolves canonically and only junk reaches the old body — but the deviation
   is real and unrelated to enum identity.
3. **The registrar should be split.** Essentials calls
   `register_p71_thread_extras` for its `ThreadGroup` half; the `Thread$State`
   half is collateral. Separating the two would remove this class of accident
   for real-JDK mode without needing a runtime mode query.
4. **Generalise the sweep.** The twenty-enum identity probe is scratch-only. A
   census asserting that no real JDK enum's `values`/`valueOf`/`<clinit>` triple
   is registered under `--jdk-only` would close the species, not just its two
   known members.

---

# 9. Verification pass, 2026-08-12 — both fixes are present and they DO assert identity

**Read-only. Nothing built, nothing run.** This section re-checks the *claims*
of §5 and §8.3 against the tree rather than the dates on them, because a
record's hypothesis can be wrong and not merely stale. Three of the four checks
confirm the record; the fourth finds a live hole in the coverage §6 claims.

## 9.1 The landed code matches what §5 and §8.3 describe

* `native-builtins/src/stack_walker.rs::option_constant_names` derives the
  constant list from `ctx.declared_fields(class_id)` filtered by
  `f.is_static && f.descriptor == format!("L{class_name};")`, in declaration
  order, and falls back to the historical three names **only** when the class
  is unresolvable or declares none. `native_option_clinit` writes `name` and
  `ordinal` into the two slots resolved against `java/lang/Enum` (never the
  receiver), and builds `$VALUES` by **re-reading each published static**, so
  `values()[i] == <the constant>` holds by construction rather than by a cached
  reference. §5 is accurate line for line.
* `native-builtins/src/lang_system.rs` carries `canonical_enum_constant` and
  `canonical_enum_values` with the gating §8.3 states: `canonical_enum_values`
  returns `None` unless the class declares at least one constant **and every
  one reads back `Value::Object(Some(_))`**, so no array with null holes is
  ever built, and `ensure_class_initialized`'s `Ok` is explicitly not treated
  as proof a real class answered. `phases_late/concurrent.rs` calls both, for
  `java/lang/Thread$State`, ahead of the old minting bodies. §8.3 is accurate.
* One naming correction for anyone searching: there is no
  `OPTION_FALLBACK_CONSTANTS` symbol. The fallback is a three-element array
  literal inside `option_constant_names`; the derivation is the function.

## 9.2 The fixture asserts IDENTITY, not non-null — checked, and it holds

This is the check the record most needed, because two defects survived this
year behind `!= null` and `values().length == 3`. `realEnumsAreSelfConsistent()`
in `regression-suite/src/RJdkStrict.java` asserts, per declared constant `c` at
index `i` — where the declared list comes from the class's **own**
`getDeclaredFields()` filtered by `f.getType() == k`, so nothing is named or
counted in the fixture:

```java
check(values[i] == c, ...);            // reference identity
check(shared[i] == c, ...);            // getEnumConstants() identity
check(valueOf.invoke(null, n) == c, ...);
check(n.equals(((Enum<?>) c).name()), ...);
check(((Enum<?>) c).ordinal() == i, ...);
```

Those are `==` on object references, over both accessors and `valueOf`. They
are exactly the shapes §8.1 measures as the *only* detectors of the
`Thread$State` defect — a `switch` and an `EnumSet` both pass over minted
constants. So the answer to "does the landed fix assert identity" is **yes, and
in the direction that catches the harder half**, plus the two null-check-passing
shapes (`name()`, `ordinal()`) that catch §2's nameless constants.
`java.lang.Thread.State` and `java.lang.StackWalker.Option` are both in the
class list.

## 9.3 The RESIDUAL: the Rust unit test cannot fail if the fix is reverted

`option_clinit_populates_enum_values_array`
(`native-builtins/src/stack_walker.rs`, test module) does now assert `name`,
`ordinal` and `$VALUES`-vs-static identity, as §6 says. But its fixture
declares exactly

```text
RETAIN_CLASS_REFERENCE, SHOW_HIDDEN_FRAMES, SHOW_REFLECT_FRAMES, $VALUES
```

— **the same three names, in the same order, as the hard-coded fallback list**
`option_constant_names` returns when it can read nothing. Delete the entire
`declared_fields` derivation and this test stays green: both paths produce the
identical three constants, and the test then asserts `array_length == 3`
against a hard-coded 3.

So the test covers the `name`/`ordinal`/identity half of the fix and is
**mutation-blind to the version-proofing half**, which is the half §5 argues is
the point ("adding `DROP_METHOD_INFO` as a fourth hard-coded name would have
re-armed the same trap"). The fixture that read green through this whole defect
was a `length == 3` (§3.2); this one is a different assertion with the same
blind spot in it.

**Fix (nominated, not applied — the file is not this pass's):** give the mock
fixture a **fourth** constant, `DROP_METHOD_INFO`, between
`RETAIN_CLASS_REFERENCE` and `SHOW_REFLECT_FRAMES` (the real JDK 25 declaration
order), and assert the array length against the fixture's own declared count
rather than a literal. The fallback list cannot produce four names or that
order, so the derivation becomes the only way the test can pass — and the
ordinals then differ between the two paths, which is the second thing the
current fixture cannot see.

## 9.4 What this pass did not check

No build and no run, so nothing here observes the repaired native's output;
§7.3 and §8.5.1 remain the open items they were. The `--jdk-only` question in
§7.4 — whether a native fabricating enum constants over a real, loaded,
runnable JDK class should be refused outright rather than repaired — is
untouched and is still the right question.
