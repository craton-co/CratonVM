# `-ea` was parsed and thrown away, so `assert` never fired

**Status:** FIXED (2026-08-12). The launcher gap was only the first of **four**
defects between `java -ea` and a working `assert`; the other three were latent
`java.lang.invoke` shim bugs that nothing could observe while the flag was being
discarded. Found while working [investigate-batch-08.md](../../../known-issues/netty/investigate-batch-08.md).

## Symptom

Two batch-08 classes failed 6 tests each on CratonVM:

- `io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest`
- `io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest`

```
org.opentest4j.AssertionFailedError: Expected java.lang.AssertionError to be thrown, but nothing was thrown.
    at io.netty.handler.codec.http2.DefaultHttp2RemoteFlowControllerTest.invalidWeightTooBigThrows(...:952)
```

The tests exercise netty's argument validation, which is written with Java
`assert` statements, so they only pass with assertions enabled.

## Why this was invisible

**The netty harness does not pass `-ea`.** Without it HotSpot fails these same
12 tests identically, so the suite run recorded "FAIL on both" and the classes
looked like an environment problem rather than a VM one. Adding `-ea` separates
them:

| | without `-ea` | with `-ea` |
|---|---|---|
| HotSpot JDK 25 | ok=28 failed=6 | **ok=34 failed=0** |
| CratonVM (before) | ok=28 failed=6 | **ok=28 failed=6** |

HotSpot honoured the flag and went green. CratonVM ignored it and did not move.
The harness gap was hiding a genuine CratonVM defect behind a matching HotSpot
failure — the inverse of the usual "harness gap inflates the bug list".

## Root cause — a chain of four

### 1. The launcher discarded the flag

CratonVM **does** implement assertion status. `native_assertion_status` backs
both `Class.desiredAssertionStatus()` and `desiredAssertionStatus0(Class)`, and
`assertion_status_default()` (`native-builtins/src/lib.rs`) returns 1 when the
`CRATONVM_ENABLE_ASSERTIONS` flag is set. With it on, `<clinit>` stores
`$assertionsDisabled = false` and real `assert` bytecode throws.

The command line never reached that switch. `normalize_java_launcher_argv`
(`vm-cli/src/main.rs`) dropped the whole assertion family on the floor with a
comment claiming assertion checking was not implemented. Only the env var
worked, which no Maven Surefire or Gradle fork will ever set. (Surefire forks
the test JVM with `-ea` by default — so this affected far more than netty.)

### 2. `MethodHandleNatives.getNamedCon` was unimplemented

The moment the flag reached the switch, **every `-ea` run died before the first
test**:

```
UnsatisfiedLinkError: java/lang/invoke/MethodHandleNatives.getNamedCon(I[Ljava/lang/Object;)I
    at java/lang/invoke/MethodHandleNatives.verifyConstants(MethodHandleNatives.java:197)
    at java/lang/invoke/MethodHandleNatives.<clinit>(MethodHandleNatives.java:221)
```

`MethodHandleNatives.<clinit>` ends with `assert(verifyConstants())`, and
`verifyConstants` is `getNamedCon`'s only caller anywhere in the JDK — so with
assertions off the native was unreachable and its absence cost nothing.

### 3. `getMemberVMInfo` answered with the wrong types

Next:

```
ClassCastException: class java.lang.Integer cannot be cast to class java.lang.Long
    at java/lang/invoke/MemberName$Factory.resolve(MemberName.java:971)
```

`MemberName.vminfoIsConsistent` — again the **only** caller of
`MethodHandleNatives.getMemberVMInfo` in the JDK, and again assert-only — reads
the answer as `{ Long vmindex, Object vmtarget }` and requires `vmtarget` to be
a `Class` for field kinds. CratonVM boxed an `Integer` and always returned the
`MemberName`.

### 4. `MethodHandleNatives.resolve` never filled in the `ACC_*` flags

Then:

```
AssertionError: arity mismatch: arguments.length=1 == function.arity()=2
  in t851:L=DirectMethodHandle.allocateInstance(a0:L)
    at java/lang/invoke/LambdaForm$Name.<init>(LambdaForm.java:1310)
```

`MemberName.flags` is `refKind<<24 | IS_METHOD/IS_FIELD/IS_CONSTRUCTOR | ACC_*`.
The Java-side constructors set the kind and the reference kind and pass `0` for
the modifiers, precisely because HotSpot's `MHN_resolve_Mem` overwrites them
with the resolved member's real access flags. CratonVM's resolve never did, so
**every** MemberName resolved through it reported `isStatic() == false` — and

```java
public MethodType getInvocationType() {
    MethodType itype = getMethodOrFieldType();
    ...
    if (!isStatic())  return itype.insertParameterTypes(0, clazz);
```

prepended a phantom receiver parameter to every static method's type. Unlike
(2) and (3) this one was **not** assert-only: the wrong `isStatic()` was there
on every run. Only its consequences were invisible.

The same fix has a second half: `MethodHandle.linkToSpecial` and the rest of the
signature-polymorphic family are declared `(Object...)Object` while a
`MemberName` for one carries the *call site's* type (`(L,L)V`), so no descriptor
match exists. A name declared exactly once in the class is unambiguous and is
taken as the match; with real overloads present the lookup declines rather than
guess.

## Fix

`vm-cli/src/main.rs`, scanned at the single launcher call site in `main` and
applied as a `VmFlags` override — **not** from inside `normalize_java_launcher_argv`,
which is pure, has ~60 unit tests, and runs long after the flag snapshot is
latched (`FLAGS: OnceLock<VmFlags>`):

- `-ea` / `-enableassertions` / `-esa` / `-enablesystemassertions` → enable
- `-da` / `-disableassertions` / `-dsa` / `-disablesystemassertions` → disable
- scoped forms (`-ea:some.pkg...`, `-ea:some.Class`) → still ignored, and named
  under `CRATONVM_DBG_ARGS` so the silence is discoverable

`-da` has to make `CRATONVM_ENABLE_ASSERTIONS` *absent*, not `0`: the flag is
parsed with `present`, under which `=0` reads as enabled. That needed
`VmFlags::from_env_with_overrides_and_unsets`, since a `MapSource` overlay can
only add.

The two scopes are tracked separately and OR-ed rather than reduced by plain
last-wins. HotSpot's `-ea` and `-esa` are independent switches; CratonVM has one
global, and last-wins would make `-ea -dsa` resolve to *off* — silently undoing
the `-ea` a build tool put there on purpose.

Plus the three `native-builtins` repairs above:
`MethodHandleNatives.getNamedCon` (registered; returns 0 with `name[0]`
untouched, which is the JDK loop's own terminator — CratonVM keeps no
counterpart of HotSpot's `MN_*`/`REF_*` table to cross-check, and echoing the
JDK's own numbers back would make the assertion certify an agreement that was
never checked), `getMemberVMInfo` (a real `Long`, and the declaring `Class` as
`vmtarget` for field kinds), and `MethodHandleNatives.resolve` (ORs the resolved
member's access flags into `flags`).

## Result

Both classes now match HotSpot test-for-test:

```
                        HotSpot -ea: found=34 started=34 ok=34 failed=0   (each class)
CratonVM before the fix: found=34 started=34 ok=28 failed=6
CratonVM after  the fix: found=34 started=34 ok=34 failed=0
```

`Http2MultiplexTransportTest` (the EC/TLS page's class) is unchanged at
`found=11 started=9 ok=6 failed=0 aborted=3 skipped=2` on all three of
{pristine dev, this branch, this branch with `-ea`}.

### Suite validation

Defect (4) is the one of the four that is **not** assert-only — it changes
`MemberName.isStatic()` on every run — so it needed its own control. A 123-class
sample of `testlist.txt` (every 6th entry), three arms interleaved per class so
host-load drift hits all three equally:

| arm | binary | flags |
|---|---|---|
| ctl | pristine `origin/dev` | — |
| mine | this branch | — |
| mine+ea | this branch | `-ea` |

**One** row of 123 differed, `io.netty.channel.nio.NioEventLoopTest`, and it was
the *control* arm that was short a test. Re-run 16 times ABBA-interleaved
(ctl/mine/mine/ctl × 4) it is `ok=12 failed=1` on both binaries every time — a
flake in the sweep's control run, not a difference. Every other class scored
identically on all three arms, `-ea` included.

Three test failures were observed in the Rust suites while validating —
`fdlibm::tests::atan2_matches_hotspot_strictmath_bit_for_bit` and
`…ieee_remainder…` in `cratonvm-types`, three `logmanager::tests::t19_h3_*` in
`cratonvm-native-builtins`, and `every_backend_door_goes_through_the_admission_gate`
in `cratonvm-cli`'s `jit_compile_gate_doors`. All were re-run on a pristine
`origin/dev` build and fail there identically; none are from this change.

## Coverage

`vm-cli/src/main.rs`: `assertion_flags_are_stripped_before_clap` (the pre-existing
behaviour — clap cannot parse `-ea`, which it reads as the cluster `-e -a`) plus
five tests over `launcher_assertions_requested`: enable/disable, no-flag →
`None` (so a plain command line cannot clear an inherited export), scoped forms
not honoured, last-wins within a scope, `-ea -dsa` staying on, and `-ea` after
the `--` program-args separator being the program's own.

## Deliberate divergences from HotSpot

Measured with the same probe on both VMs (`Class.desiredAssertionStatus()` and
whether a real `assert` throws). Ten of twelve rows agree; the two that do not
are the known cost of one JVM-wide switch:

| flags | CratonVM | HotSpot |
|---|---|---|
| `-esa` | **on** | off (system classes only) |
| `-ea:SomeClass` | **off** | on |

A lone `-esa` asks for assertions in bootclasspath classes; CratonVM turns them
on everywhere, which is over-broad in the direction that fails loudly rather
than silently. A scoped `-ea:pkg` is refused rather than promoted to global —
promoting it would switch on assertions for exactly the classes the caller
deliberately excluded. Closing either properly means per-class assertion status
(the `ClassLoader.retrieveDirectives` classes/packages arrays, currently
returned empty), which is a feature, not a repair.

## Harness note

`apps/netty-suite-runner/common.args` still does not pass `-ea`; that harness is
under the gitignored `apps/` tree and is shared live with every other agent on
the build host, so it is not changed here. Passing `-ea` is what Maven Surefire
does for netty's own CI, and with the VM fix landed it is now safe to add — the
sequencing warning in the original write-up (doing it *before* the VM fix would
turn 12 matching failures into 12 CratonVM-only ones) no longer applies.

The three `NativeImageHandlerMetadataTest` classes on the same batch-08 page fail
on both VMs for an unrelated harness reason and are **not** VM defects:

```
Native Image reflection metadata is required ... not found under
  .../META-INF/native-image/null/null/generated/handlers/reflect-config.json
```

The `null/null` is the giveaway — the test builds that path from Maven
group/artifact system properties that Surefire sets and the fork-per-class
runner does not.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
<cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m -ea \
    @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest
```
