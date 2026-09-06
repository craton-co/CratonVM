# The strict corpus had no Windows key — and two mode-independent defects were sitting behind the leg nobody ran

**Status: FIXED-MEASURED 2026-09-05.** Windows 11, JDK 25 (Temurin
`25.0.3+9`, the same image as the oracle), release binary built from this
branch. Both defects are fixed and re-measured on a binary carrying the fix;
the `25-windows` baseline is minted and the blocking CI step now runs on the
Windows leg.

**Lane** `claude/jdk-only-finish-20260905`, branched from `origin/dev` at
`37ec2cfd0`.

---

## 1. The leg that could not run, and why that is not a small thing

`scripts/jdk-only-strict-probes.sh` is acceptance criterion 6 of
`docs/feature-designs/jdk-only-mode.md` §11 — *"the strict corpus is green"* —
and `ci.yml`'s `build-and-test` job runs it **blocking**. It ran on
`ubuntu-latest` only, and the comment above the step said exactly why:

> `ubuntu-latest` only, exactly like the bridge ratchet and for the same
> reason: `25-linux` is the only committed key, and on the windows leg the
> gate would (correctly) refuse with exit 2 rather than look green.

That was the right call at the time and it is the reason this record exists.
The baseline is keyed `<jdk-feature>-<os>` because the transcript is a property
of the image and the platform — so with no `25-windows` file, **no gate anywhere
in the repository compared this VM to HotSpot on Windows.** The advisory
`jdk-only` job schedules the same script on `windows-latest`, and it refused on
that leg for the same missing-key reason, so `continue-on-error` was not even
the binding constraint.

Running it here found two divergences, and both are **compatibility** defects —
present in `--real-jdk` as well as `--jdk-only`. Strict mode did not introduce
either; the Windows leg is simply where they were visible.

```text
                                        HotSpot     CratonVM (both modes)
security … p12=                         true        false
agent    … attach=                      list-ok     throw-UnsatisfiedLinkError
```

---

## 2. Defect 1 — `KeyStore.setEntry(SecretKeyEntry)` stored nothing, and said nothing

### The reading

`apps/probes/JdkOnlyPlatformProbe.java`'s `security` section does a PKCS#12
round trip: store a secret key, serialise to a byte array, read it back,
compare the material. `p12=false` means it came back different or absent.
Shrinking it to `KeyStore` calls alone:

```text
                              HotSpot    CratonVM   (identical in both modes)
size() before                    0          0
setEntry("secret", …)      returns    returns          <-- no exception either way
size() after                     1          0
containsAlias("secret")       true      false
isKeyEntry("secret")          true      false
getEntry("secret", pw)     <entry>       null
getKey("secret", pw)         <key>       null
store(baos, pw) length         421        122          <-- an EMPTY keystore
setKeyEntry("s2", key, pw, null); size()
                                 1          1          <-- the pre-1.5 route WORKS
```

The last row is the discriminator. Both routes reach the same SPI object
through the same delegator, so "PKCS12 is broken" does not fit — one entry
point works and its twin does not.

### The cause

`engine_set_entry` is the one `engine*` callback in `native-builtins/src/keystore.rs`
that read its store id through `keystore_ensure_store_id`, which **unwraps**.
Its fifteen siblings all use `get_store_id(ctx, this)` directly. The unwrap is
written for the public `java.security.KeyStore` wrapper:

```rust
fn unwrap_keystore_spi(ctx, keystore_obj) -> ObjectRef {
    if let Value::Object(Some(spi)) = ctx.get_field_by_name(keystore_obj, "keyStoreSpi") { return spi; }
    // Fallback: real KeyStore's field order is type(0)/provider(1)/keyStoreSpi(2).
    if ctx.object_num_fields(keystore_obj) > 2 {
        if let Value::Object(Some(spi)) = ctx.get_field(keystore_obj, 2) { return spi; }
    }
    keystore_obj
}
```

But `engine_set_entry`'s receiver **is already the SPI**. For
`KeyStore.getInstance("PKCS12")` that SPI is
`sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12`, a
`sun/security/util/KeyStoreDelegator`, which has no `keyStoreSpi` field — so the
name lookup missed and the blind slot-2 fallback fired. Slot 2 of
`KeyStoreDelegator` is `primaryKeyStore`:

```java
private final String primaryType;                        // 0
private final String secondaryType;                      // 1
private final Class<? extends KeyStoreSpi> primaryKeyStore;   // 2   <-- a Class MIRROR
```

So every `setEntry` was filed under the store id of a **`java.lang.Class`
mirror** — a process-global object no reader ever asks about. The write did not
even survive: the run's own shutdown census had already said so, in a line that
reads like noise until you know what to look for.

```text
[cratonvm] descriptor-coercion census: total=19 primitive-into-reference[read=16 store=3]
```

Three `Value::Int` stores into reference-typed slots, destroyed — `set_store_id`
stamping an id onto a `Class` mirror that has no such field.

### The fix

Two edits, both in `keystore.rs`:

1. `unwrap_keystore_spi`'s slot-2 fallback now **checks what it produced**: the
   candidate must be a `java/security/KeyStoreSpi`. The index is blind, so its
   answer has to be believed only when it is right. When
   `java/security/KeyStoreSpi` cannot be resolved at all — a `--synthetic-jdk`
   image — the historical behaviour is kept rather than silently changed on an
   image that cannot answer the question.
2. `engine_set_entry` reads its id off `this` (`ensure_store_id_on`), which is
   the invariant its fifteen siblings already hold.

Edit 1 alone fixes the defect; edit 2 is what stops it coming back the next time
someone reaches for the unwrapping helper inside an `engine*` callback.

**The gate is three mock-driven tests** (`unwrap_keystore_spi_tests`), not a
source scan, because a source witness pins today's spelling rather than the
behaviour. The second test is the one that keeps the first honest: a guard that
rejected *everything* would make the delegator case pass while breaking every
real `java.security.KeyStore` wrapper — which is the caller the fallback exists
for, and whose failure mode `keystore_id_from_object` already documents (a
truststore silently replaced by the platform roots, so every peer certificate
signed by the caller's own CA is rejected as `UnknownIssuer`).

---

## 3. Defect 2 — the Windows attach provider's `tempPath()` had no bridge

```text
java.lang.UnsatisfiedLinkError: sun/tools/attach/AttachProviderImpl.tempPath()Ljava/lang/String;
    at sun.tools.attach.AttachProviderImpl.isTempPathSecure(AttachProviderImpl.java:80)
    at sun.tools.attach.AttachProviderImpl.listVirtualMachines(AttachProviderImpl.java:64)
    at com.sun.tools.attach.VirtualMachine.list(VirtualMachine.java:146)
```

`VirtualMachine.list()` is what every JVM-enumerating tool calls. On Linux it
has always worked here: `sun.tools.attach.AttachProviderImpl` there declares no
native on that path and delegates to `HotSpotAttachProvider.listVirtualMachines()`,
ordinary bytecode. **The Windows class of the same name is a different class**
(`javap -p --module jdk.attach sun.tools.attach.AttachProviderImpl`):

```text
private static native String  tempPath();
private static native long    volumeFlags(String);
private static native int     enumProcesses(int[], int);
private static native boolean isLibraryLoadedByProcess(String, int);
```

and `listVirtualMachines()` picks its path with the first two:

```text
0: invokestatic  isTempPathSecure:()Z
3: ifeq          11
6: invokespecial HotSpotAttachProvider.listVirtualMachines   <-- what Linux runs
11: invokevirtual listJavaProcesses                          <-- needs the other two
```

**Fixed** by `native-builtins/src/attach_provider.rs`, a `#[cfg(windows)]`
module registering exactly two `Bridge` natives: `tempPath()` (the platform's
`GetTempPath`, which is what `std::env::temp_dir()` calls — not a re-derivation
from `java.io.tmpdir`, which a program can set and which would make a security
verdict depend on a system property) and `volumeFlags(String)`
(`GetVolumeInformationW`'s file-system flags word, handed back whole; the JDK's
own bytecode does the `FILE_PERSISTENT_ACLS` masking).

**Why two and not four, stated so nobody reads it as an oversight.** Answering
the first two truthfully routes the call onto the same bytecode the Linux leg
already runs green, so the platforms converge on one path instead of this VM
growing a second, Windows-only process enumeration. `enumProcesses` /
`isLibraryLoadedByProcess` are reached only when `%TEMP%` is *not* on an
ACL-bearing file system, and a fabricated answer for either would be a list of
processes this VM invented. They stay unregistered, so that path still fails
loudly and names itself.

The registration is `#[cfg(windows)]` at both the module and the call site
because the **Linux** class declares neither method: a `Bridge` naming a method
the runtime image does not have is precisely what `regression-suite/bridge-ratchet.sh`
exists to catch.

```text
after:  cratonvm --jdk-only … AttProbe   ->   list=[]
```

An empty list, which is what `HotSpotAttachProvider` answers here and what the
Linux leg has always answered. The probe's assertion is `list instanceof List`.

**The path this now routes onto is a path that has already been debugged.**
`java-agent-transformer-never-fires-and-attach-list-throws-FIXED-20260806.md`
(in the internal tree, indexed by basename)
is the same probe row going red on **Linux** in August, for an unrelated reason
— `HotSpotAttachProvider.listVirtualMachines` died inside `Files.getAttribute`
on the platform filesystem, and that was fixed there. So converging Windows
onto `HotSpotAttachProvider` rather than onto `listJavaProcesses` puts both
platforms on the code that record already paid for, which is the argument for
two natives rather than four stated a second way.

---

## 4. Defect 3, in the gate itself — the JNI section had switched itself off

`scripts/jdk-only-strict-probes.sh` builds its JNI fixture with
`cc -shared -fPIC`, a GCC/Clang spelling. A Windows host has no `cc`, so the
script took its `jni-lib-no-cc` degradation and the `jni` section reported
`lib=absent` in **all three arms** — the self-disabling shape the script's own
header rejects in so many words, arriving on the platform nobody had run it on.

It is worse here than that header says. The script's own comment records that
`apps/probes/` is scheduled by nothing else (`grep -c 'probes/' regression-suite/run.sh`
is 0), so with no compiler the JNI boundary under `--jdk-only` on Windows was
covered by **nothing, anywhere** — and the run would have said PASS if a
baseline had existed.

Fixed by an MSVC fallback: `vswhere.exe` (a fixed, versionless path shipped by
every VS installer since 2017) → `vcvars64.bat` → `cl /LD`. This adds no
prerequisite; it spells one that was already required, since rustc's
`x86_64-pc-windows-msvc` target links with the same toolchain. On failure it
explains itself into the same `jni-build.log` and returns 1, so the caller
falls through to the unchanged declared-degradation path — this can turn a
refusal into a pass, never a pass into a refusal.

**One quoting bug cost a run and is worth naming**, because it does not look
like one. `/Fo:"<dir>\"` is the obvious spelling and it is wrong: a backslash
immediately before a closing quote escapes the quote, so `cl` received one
merged argument and answered `D8003: missing source filename` — a compiler
error that reads like a broken source file. The batch now `cd`s into the output
directory and names the DLL relatively.

**With the section actually running, it is clean.** The whole JNI boundary —
primitive call ABI including 64-bit and double returns, string marshalling,
array pinning and commit-back, object arrays, field get/set, native→Java
re-entry, a pending exception surviving an up-call, `ThrowNew`, and
`RegisterNatives` binding — is byte-identical to HotSpot on Windows in **both**
modes:

```text
jni mapped=cratonjniprobe.dll add=42 mul=6442450941 scale=6.0 rev=notarc arrSum=15
    abortKept=[1, 2, 3, 4, 5] doubled=[2, 4, 6, 8, 10] join=a|b|c field=7->21
    upcall=27 upcallThrow=iae throw=ISE:from-native registered=true
```

That is a result, not a formality: it is the first evidence anywhere that the
JNI boundary agrees with HotSpot on this platform.

---

## 5. The baseline, and the one line that was added by hand

With all three fixes, the three probes are byte-identical to HotSpot in both
modes. The `vthreads` section is the exception, and it is the same intermittent
one the `25-linux` baseline carries. **Ten three-arm runs, same binary, same
image, back to back:**

| divergent set | runs |
|---|---:|
| *(none)* | 6 |
| `JdkOnlyPlatformProbe/strict/vthreads` | 3 |
| `JdkOnlyPlatformProbe/real/vthreads` | 1 |

The divergence is always `handoffs=61..63` and once `allJoined=false` against
HotSpot's `handoffs=64 allJoined=true` — a race in virtual-thread handoff
counting. It appears on both arms and it is the only section that moves, so it
is not a mode effect.

`--update-baseline` regenerates the whole set from **one** run. The generating
run happened to catch the `real` arm, so committing exactly what it wrote would
have gone red on three of the other nine — and a gate that flakes is a gate that
gets switched off. `strict/vthreads` was therefore added by hand, with the
tally above written into the file, which is the same hand-edit-with-a-reason the
`25-linux` baseline documents in the other direction.

Scored against it, on a fresh run:

```text
divergent sections: 0 observed, 2 baselined
RESULT: PASS -- every arm completed and no section diverged that the
        baseline does not already carry.
```

---

## 6. What changed in CI

The blocking `Strict corpus ratchet` step in `build-and-test` moves from
`matrix.os == 'ubuntu-latest'` to `matrix.os != 'macos-latest'`. macOS stays
out: there is no `25-macos` key and minting one needs a run on that platform,
not a copied file. The advisory `jdk-only` job needs no edit — it already
schedules the script on all four legs, and its `windows-latest / 25` leg stops
refusing the moment the baseline exists; its comment saying `25-linux` is the
only committed key is corrected.

**What I could not verify from here, stated rather than assumed.** The MSVC
fallback is measured on this host only. On `windows-latest` the runner has both
`vswhere.exe` at the standard path and the VC x64 tools, so the path should
resolve — but if it does not, and no `cc` is on PATH either, the step REFUSES
(exit 2) and the job goes red rather than silently passing. That is the correct
direction and it is loud; the comment on the step says so and says to fix the
fixture rather than reach for `ALLOW_DEGRADED_FIXTURES`, which would switch the
JNI coverage back off.

---

## 7. The three suite arms on the merged tree, and the census nobody had taken here

Landing protocol §5 step 2, run twice on release builds of the merged tree —
Windows, JDK 25. The second is the tree that lands.

```text
                          dev merged at aeaaf87e9      dev merged at e8e68a486
CRATONVM_ARGS=--jdk-only   129 passed, 1 failed         131 passed, 0 failed
SUITE=all                  130 passed, 0 failed         131 passed, 0 failed
SUITE=core                  90 passed, 0 failed          91 passed, 0 failed
```

**Both columns are reported, and the left one is the more useful.**

### `RMapGcStress` is a HARNESS DEADLINE on a loaded host — and I read it wrong twice before that

The left column's failure looked like the concurrency leakage
`the-suite-ab-that-was-the-harness-20260902.md` recorded: it failed in one arm,
passed in `SUITE=all` which schedules the identical vectors, and passed alone.
On a later cycle it failed in **all three arms at once**, on a merge that had
just brought `gc/src/heap.rs`, `gc/src/vm_heap.rs` and `types/src/heap_types.rs`
— a much more alarming shape, and one that reads as a GC regression.

It is neither. **Every failure of it measured here was `rc=124`**, and the
harness says what that is in the failure line itself:

```text
RMapGcStress FAIL rc=124: HARNESS FAULT — TIMED OUT; the harness killed
                  the VM, it did not fail [try TIMEOUT=600]
```

`regression-suite/run.sh` defaults to `TIMEOUT=120`. This host is Windows with
about 700 MB free while a fat-LTO `rustc` holds 6 GB, and the GC-stress vectors
do not finish inside 120 s under that pressure. The control is one command and
it is decisive:

```text
ONLY=RMapGcStress  TIMEOUT=600  --jdk-only
  pre-GC-merge binary    PASS      (the control)
  post-GC-merge binary   PASS      (the same binary that had just failed at 120)
```

So the GC merge is exonerated, and so is the concurrency story: the vector needs
more than 120 s here — in every mode, on every binary, alone or scheduled.
`RMapResizeGc`'s one failure is the same `rc=124` and clears the same way.

**Two readings were wrong before this one, and both were wrong in the
flattering direction** — "a known intermittent someone else already
adjudicated" and "another lane's GC change" are each easier to believe than "my
host is too slow for the default deadline". What settled it was reading `rc`
instead of the PASS/FAIL label, and running the previous binary as a control on
the same loaded host. The arms are therefore run at `TIMEOUT=600`, the value the
harness itself names.

The strict arm prints its own census, and this is the first time it has been
taken on Windows. Over 131 vectors, union by triple:

```text
native-shadows-bytecode   1494 native-won   ·   483 bytecode-won
                                                (27 of those NEVER native — the
                                                 contract working; the rest also
                                                 ran the native in another vector)
synthetic-native-registered  1645
interpreter_shadow_unenforced 11376
compatibility_classes            0
saturation: none — every bounded collection reported truncated: false
```

**`compatibility_classes: 0` across the whole corpus**, with `saturation: none`
so it is a total rather than a floor. That is the definition-of-done predicate,
which `the-definition-of-done-run-on-the-three-real-workloads-20260828.md`
established on five workloads **on Linux**. It holds on this platform too, on
131 vectors. It is not the DoD itself — none of those three workloads is checked
out here — but it is the predicate, on a platform where it had never been read.

---

## 8. What this does NOT claim

* **Not a stage advance.** `docs/jdk-only-migration.md` §"Rollout stages" is
  untouched. Stage 3 wants "Windows filesystem/process/networking vectors
  stable" across JDK 21 and 25; this is three probes on JDK 25, and the JDK 21
  legs still refuse for want of a `21-*` key.
* **Not a statement about `--synthetic-jdk`.** All measurement here is on the
  two shipping arms.
* **Two of the three fixes are mode-independent.** They were found by a
  `--jdk-only` instrument and they are not `--jdk-only` defects. The pattern is
  the campaign's usual one read from the other side: strict mode is where the
  default's defects become reachable.
* **The `vthreads` race is not fixed and not diagnosed here.** It is baselined
  on both platforms and it is somebody's lane.
