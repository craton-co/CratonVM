# W7-93 — `StackWalker$Option`'s constants: one null, three nameless, and a native `<clinit>` that froze the JDK-21 shape

**Status: SOURCE LANDED 2026-08-12, NOT BUILT and NOT RUN.** The measurements in
§1–§3 are real runs of the pristine `dev` control binary (`44044c7e2`) and of
HotSpot JDK 25 on this host, taken by this lane. Everything about the *fix* in
§5 is source-only: this lane was not permitted to build, so no claim that the
repaired native produces the numbers §6 asserts has been observed. The fixture
half **is** verified in both directions — it passes on HotSpot and fails on the
control at the intended assertion (§6).

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

docs/internal/fixed-suite-bugs/elasticsearch-suite/stackwalker-option-enum-constants-null-blocks-es-suite-FIXED.md
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
