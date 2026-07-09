# BC asn1-regression `PKCS12Test` StackOverflowError (FIXED)

Status: fixed

Date observed: 2026-07-09, timing CratonVM against HotSpot for the
`apps/bc-java` core-module suites (Azure build host, `dev` @ `885e936c`).

Date fixed: 2026-07-09, branch
`codex/fix-bc-pkcs12-stackoverflow-20260709-001`.

## Fix

Root cause was the base `java/io/InputStream.read([BII)I` native fallback in
`native-io/src/lib.rs`. For non-`ByteArrayInputStream` receivers it checked
whether the receiver class declared its own three-arg `read` override and then
called that override again. That is wrong once the base method has already been
selected by an explicit superclass call.

Bouncy Castle's `IndefiniteLengthInputStream.read([BII)` intentionally calls
`super.read([BII)` for small/default reads. CratonVM re-dispatched that
super-call back to `IndefiniteLengthInputStream.read([BII)`, creating the
unbounded recursion seen in `PKCS12Test`.

The fix removes that redispatch. Once the base `InputStream.read([BII)` native
is entered for a non-BAIS receiver, it now stays on the JDK default
implementation: loop over the receiver's virtual `read()I`, copy bytes, and
return the count or `-1`. Normal virtual calls to a subclass's three-arg
override still resolve before this native is entered.

## Summary

`org.bouncycastle.asn1.test.RegressionTest` — the `core` module's ASN.1
regression suite — aborts immediately under CratonVM with a real
`StackOverflowError` while running `PKCS12Test`, well into deep recursion in
`IndefiniteLengthInputStream.read()`. HotSpot runs the full suite (including
`PKCS12Test`) cleanly in 0.5s. Because `RegressionTest.main()` has no
per-test try/catch (unlike the `SimpleTest`-based suites, which print
`<Name>: <failure>` and continue), the uncaught exception kills the whole
suite — every test after `PKCS10` never runs.

This is a genuine CratonVM correctness/capacity gap, not a harness or
methodology artifact: the same classpath, same `--Xmx`, and (see below) a
much larger native stack budget than HotSpot's default all reproduce it.

## Evidence

```
CV=/data/data/wt-bc-suite-bench-20260709/target/release/cratonvm-bcbench
JDK=/home/victor/jdk25
CP=apps/bc-java/core/build/classes/java/main:apps/bc-java/core/build/classes/java/test:apps/bc-java/core/build/resources/main:apps/bc-java/core/build/resources/test

$CV --java-home $JDK --stack-dump-on-timeout 0 --Xmx 1g -cp "$CP" \
  org.bouncycastle.asn1.test.RegressionTest
```

Output (last ~16,400 lines are the same repeated frame):

```
InputStream: Okay
EqualsAndHashCode: Okay
Tag: Okay
Set: Okay
ASN1Integer: Okay
DERUTF8String: Okay
Certificate: Okay
Generation: Okay
OCSP: Okay
OID: Okay
RelativeOID: Okay
PKCS10: Okay
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/StackOverflowError
	at org/bouncycastle/asn1/test/RegressionTest.main(Unknown Source)
	at org/bouncycastle/util/test/SimpleTest.runTests(Unknown Source)
	at org/bouncycastle/util/test/SimpleTest.runTests(Unknown Source)
	at org/bouncycastle/util/test/SimpleTest.perform(Unknown Source)
	at org/bouncycastle/asn1/test/PKCS12Test.performTest(Unknown Source)
	at org/bouncycastle/asn1/test/PKCS12Test.implTest(Unknown Source)
	at org/bouncycastle/asn1/ASN1InputStream.readObject(Unknown Source)
	at org/bouncycastle/asn1/BERSequenceParser.parse(Unknown Source)
	at org/bouncycastle/asn1/ASN1StreamParser.readVector(Unknown Source)
	at org/bouncycastle/asn1/ASN1StreamParser.implParseObject(Unknown Source)
	at org/bouncycastle/asn1/ASN1StreamParser.parseImplicitPrimitive(Unknown Source)
	at org/bouncycastle/asn1/ASN1InputStream.createPrimitiveDERObject(Unknown Source)
	at org/bouncycastle/asn1/DefiniteLengthInputStream.toByteArray(Unknown Source)
	at org/bouncycastle/util/io/Streams.readFully(Unknown Source)
	at org/bouncycastle/asn1/IndefiniteLengthInputStream.read(Unknown Source)
	at org/bouncycastle/asn1/IndefiniteLengthInputStream.read(Unknown Source)
	... (repeats ~16,000 more times)
```

**Ruled out: not a tunable-stack-size issue.** `main()` runs on the primary
VM thread, which `vm-cli/src/main.rs` already sizes at 128 MiB (see commit
`e59cefdf`, "bump main-vm stack from 64 MB to 128 MB for deep recursion").
`RUST_MIN_STACK` (the knob that controls *spawned/child* Java-thread native
stacks, default 8 MiB — see `vm/src/vm/vm_exec.rs`) has **zero effect** here,
confirmed by rerunning with `RUST_MIN_STACK=134217728`: identical failure,
identical ~16,400-line trace. There is currently no CLI flag to raise the
main thread's budget past 128 MiB (the code comment notes "*A future `-Xss`
CLI flag will gate the Java stack-depth limit independently of this native
budget*" — that flag doesn't exist yet).

128 MiB / ~16,000 frames implies roughly ~8 KB of native stack per
`IndefiniteLengthInputStream.read()` interpreter frame — far more than
HotSpot needs for the same call (HotSpot's default thread stack is 512 KB-1
MB and doesn't overflow at all), consistent with the interpreter's per-frame
bookkeeping cost rather than a runaway/infinite recursion bug.

## Impact on suite comparisons

Blocks any `asn1-regression` timing comparison entirely — CratonVM never
reaches a completed/timed state for this suite, it crashes in ~1s. See the
sibling BC core-suite timing results (not yet written up as a doc) for
`math-ec` and `crypto-regression`, which instead hit CratonVM's
interpreter-throughput wall (org/bouncycastle/ is JIT-banned per
`vm/src/jit/skip_list.rs`, timeout at 600s) rather than crashing.

## Validation

- `cargo test -p cratonvm-native-io inputstream_super_read_bytes_uses_base_default_loop -- --nocapture`
  passes and proves the fallback ignores a receiver-declared three-arg override
  after the base method is selected.
- Built unique binary
  `target/bc-pkcs12-soe-20260709-001/release/cratonvm-bc-pkcs12-soe-20260709-001.exe`.
- `CRATONVM_BIN=.../cratonvm-bc-pkcs12-soe-20260709-001.exe cargo test -p cratonvm-vm --test inputstream_super_read_probe -- --nocapture`
  passes. The probe covers both the Bouncy Castle-shaped `super.read([BII)`
  recursion and the earlier Jetty-shaped normal virtual dispatch case where
  inherited `read([B)I` must still reach a subclass `read([BII)I` override.

The local checkout does not contain `apps/bc-java`, so the full
`org.bouncycastle.asn1.test.RegressionTest` suite was not rerun here. The
committed probe validates the controlling VM mechanism that produced the
`IndefiniteLengthInputStream.read()` recursion.

## Original leads (superseded)

- Reproduce standalone with a minimal indefinite-length BER fixture to
  confirm whether the recursion depth genuinely tracks input structure
  (expected: bounded, proportional to nesting) or is looping without
  consuming input (a real infinite-recursion bug) — the ~16,000-frame count
  is suspiciously round/large for hand-written PKCS12 test fixtures, which
  argues for the latter needing verification.
- If the recursion is genuinely bounded but per-frame native stack cost is
  just too high, the fix is either (a) reduce per-interpreter-frame native
  stack usage for this call shape, or (b) land the "future -Xss CLI flag"
  mentioned in `vm-cli/src/main.rs` and raise the default past 128 MiB.
