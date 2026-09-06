# A Mockito inline mock's inner self-call runs the REAL method, about 1 run in 40

## Status

**OPEN, characterised and rate-measured, root cause not pinned.** One test
method, one exception, ~2.4% of runs with the JIT on, only when the host is
under real contention. Everything else in its class passes every time.

This is the second row of the retired
`loader-jar-nested-url-connection-npe-pair-FIXED-20260905` write-up. That
page's first row (a `spy()` dropping `ZipFile`/`JarFile` native shadows) is
fixed; this row's blocking symptom on 2026-09-05/06 — `mock()` failing
outright inside Byte Buddy — was a separate JIT wrong-code regression, also
fixed. With both of those gone the ORIGINAL 2026-09-04 symptom is what is
left, and it is still here.

## The symptom

```
org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests
  getContentLengthWhenContentLengthMoreThanMaxIntReturnsMinusOne()

java.lang.NullPointerException: Cannot invoke
  "…nested.NestedUrlConnectionResources.connect()" because "this.resources" is null
	at …nested.NestedUrlConnection.connect(NestedUrlConnection.java:195)
	at …nested.NestedUrlConnection.getContentLengthLong(NestedUrlConnection.java:149)
	at …nested.NestedUrlConnection.getContentLength(NestedUrlConnection.java:142)
	at …NestedUrlConnectionTests.getContentLengthWhenContentLengthMoreThanMaxIntReturnsMinusOne(:78)
```

The test is three lines:

```java
NestedUrlConnection connection = mock(NestedUrlConnection.class);
given(connection.getContentLength()).willCallRealMethod();
given(connection.getContentLengthLong()).willReturn((long) Integer.MAX_VALUE + 1);
assertThat(connection.getContentLength()).isEqualTo(-1);
```

`this.resources` being null is CORRECT: the mock instance is built by Objenesis
with no constructor, so the `final` field is never assigned. The defect is that
the real body reaches `connect()` at all. `getContentLength()` (line 142) makes
one virtual self-call to `getContentLengthLong()`, which IS stubbed — on
HotSpot the mock answers `Integer.MAX_VALUE + 1` and the real body returns -1
without ever touching `this.resources`. On CratonVM, sometimes, that inner
call runs the real method instead.

## The rate, and the one axis that separates

Azure host 2, `spring-boot-loader`, `SbRunner` on the full class, one binary
(dev `d2cfbd543` + this session's three fixes). "Under load" means a real
`cargo build --release` or `cargo test` running on the same box; every failure
this session happened during one.

| arm | failures / runs |
|---|---|
| JIT on, cumulative across the session | **~9 / 370** (~2.4%) |
| JIT off, cumulative | 0 / 170 |
| JIT on vs JIT off, run CONCURRENTLY through one sustained release build | 1 / 70 vs 0 / 70 |

The concurrent pair is the load-matched arm — sequential arms on this host are
worthless for a load-sensitive vector, because the load moves between them.
One pair is not significant on its own; the cumulative split (every observed
failure JIT-on, none in 170 JIT-off runs) is what carries the claim, and it is
worth exactly that much: **suggestive of JIT dependence, not established.**

## Ruled out

* **Not an environment knob.** The suite runner sets four
  (`CRATONVM_REAL=net-sockets,aqs`, `CRATONVM_THREADS=-default-watchdog`,
  `CRATONVM_JIT=rootsnap-cache`, `CRATONVM_DBG_CORRUPT_CELL=1`). Bisected one
  at a time, 40 runs each: 0, 2, 1, 0 — and the control arm with NO knob set
  also failed 1/40. The knobs are noise; the load is the ingredient.
* **Not the `spy()`/redefine-immunity bug** in the sibling class: that one is
  deterministic, is fixed, and has a different exception at a different site.
* **Not `CRATONVM_JIT_IR_PHI_COPY_REGS`.** That switch's miscompile made
  `mock()` itself fail 100% of the time with a Byte Buddy
  `ArrayIndexOutOfBoundsException`; it is fixed, and this failure is what the
  test does after `mock()` succeeds.
* **Not test ordering.** The failing method is the same one every time, and
  the other ten methods in the class have never failed.
* **Not a disk-space artefact.** Reproduced with `/data` at 86% as well as at
  97%.

## Leading hypothesis (untested)

`vm/src/vm/vm_exec.rs` already records this exact failure mode for a different
class, and the note there is the best lead:

> the advice never re-entered, so Mockito's `SelfCallInfo` self-call grant was
> never consumed and leaked one step — the NEXT intercepted call was swallowed
> as a self-call and ran the real JDK body

(measured 2026-08-30 on `SimpleClientHttpResponseTests`, where a registered
`InputStream.transferTo` native won over the woven body on the `Method.invoke`
path). If ANY entry on this path skips the advice, the grant Mockito set for
`getContentLength` is still outstanding when `getContentLengthLong` arrives,
and `getContentLengthLong` is swallowed as the self-call — which is exactly the
observed shape.

What argues for it: the shape matches precisely, and the precedent is in this
VM. What argues against it: `java/net/URLConnection.getContentLength()I` and
`getContentLengthLong()J` ARE registered natives
(`native-builtins/src/net_phase_e.rs`), but `NestedUrlConnection` overrides
both, and `CRATONVM_DBG=native-shadow` on a passing run reports no
`URLConnection` row at all. A failing run has not been captured under that
census — that is the first thing to do.

## Next steps, in order

1. **Capture a failing run with the censuses armed.** Soak with
   `CRATONVM_DBG=native-shadow` (and `CRATONVM_DBG_NATIVE_ENTRY=1`) and keep
   the log of the run that fails, not of a run that passes. At ~2.4% that is
   ~40 runs under load per capture.
2. **Ask whether the advice ran.** Instrument (or trace) `MockMethodAdvice`
   entry for `getContentLengthLong` in a failing run: the question is whether
   the advice was skipped, or entered and answered "self-call".
3. **If the grant leaked, find the entry that skipped.** The precedent's fix
   was a redefine guard on one reflective-dispatch door; this one, if it is
   the same family, is a door that door's guard does not cover.
4. Only then decide whether it is JIT-dependent: if the skipped entry is a
   compiled call site, the arm split above stops being a coincidence.

## Repro

```bash
# One run (passes ~97% of the time — soak it):
CP=$(cat <module>/build/cratonvm-test-cp.txt)
cd apps/spring-boot/loader/spring-boot-loader
<binary> --java-home <jdk> --Xmx 2g --add-opens=java.base/java.net=ALL-UNNAMED \
  --stack-dump-on-timeout 0 -cp "apps/spring-boot/sb-runner:$CP" SbRunner \
  org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests
```

Run it 40-70 times WHILE a `cargo build --release -p cratonvm-cli` runs on the
same host, and count `failed=1`. An unloaded box gives 0 in 60 and proves
nothing.
