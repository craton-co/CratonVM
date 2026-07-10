# BC asn1-regression `X9Test` SIGSEGV in `array_descriptor_of` (checkcast)

Status: fixed

Date fixed: 2026-07-09

## Fix

The interpreter now keeps a popped `checkcast`/`instanceof` receiver in
`JvmThread::native_pin_roots` for the whole type-check operation, including the
array-descriptor branch. That closes the stale-reference window where the value
had left the operand stack but had not yet been pushed back. The JIT
`checkcast` helper also now validates pointer-shaped receivers with
`heap.is_object_address()` before reading the object header, matching the
existing `jit_instanceof` guard.

Touched code:

- `vm/src/runtime/interpreter.rs`
- `vm/src/jit/helpers.rs`

Verification on Windows with JDK 25:

```
cargo test -p cratonvm-vm jit_checkcast_non_heap_ptr_returns_zero -- --nocapture
target\debug\deps\cratonvm_vm-*.exe jit_checkcast_non_heap_ptr_returns_zero --nocapture

target\release\cratonvm.exe --java-home "%JAVA_HOME%" --stack-dump-on-timeout 0 --Xmx 1g -cp "<bc core main/test/resources>" org.bouncycastle.asn1.test.X9Test
target\release\cratonvm.exe --java-home "%JAVA_HOME%" --stack-dump-on-timeout 0 --Xmx 1g -cp "<bc core main/test/resources>" org.bouncycastle.asn1.test.RegressionTest
```

Results:

- `jit_checkcast_non_heap_ptr_returns_zero`: passed.
- `X9Test`: `X9: Okay`, rc 0.
- `RegressionTest`: reaches and passes `X9: Okay`; the run still reports the
  unrelated pre-existing `X500Name: Turkish locale dotless-i fold not active`
  suite failure, but there is no `array_descriptor_of` SIGSEGV.

Date observed: 2026-07-09, rerunning `apps/bc-java`'s `asn1-regression`
suite against HotSpot after landing the `InputStream` super-read
redispatch fix (`9b7f673b`, see
[`bc-asn1-pkcs12test-indefinitelengthinputstream-stackoverflow-FIXED.md`](bc-asn1-pkcs12test-indefinitelengthinputstream-stackoverflow-FIXED.md)).

## Summary

That fix resolved the `PKCS12Test` `StackOverflowError` (`PKCS12: Okay` now
prints), so `org.bouncycastle.asn1.test.RegressionTest` gets six tests
further than before — through `Misc: Okay` — but then crashes with a real
`SIGSEGV` (native core dump, no Java exception, no stack trace printed)
at the start of the next test in sequence, `X9Test`
(`core/src/test/java/org/bouncycastle/asn1/test/RegressionTest.java` lists
`MiscTest()`, then `X9Test()`). This is a **different bug** from the one
just fixed — not a regression of it — and it still blocks the whole suite,
since `RegressionTest.main()` has no per-test try/catch (see the PKCS12 doc
for that same structural point).

`X9Test` exercises ASN.1 encodings for X9 EC/financial-standard structures
(point encoding, `X9ECParameters`, etc.) — adjacent in subject matter to
`math-ec`, which is separately blocked by CratonVM's deliberate
`org/bouncycastle/` JIT ban (see
[`math-ec` timeout entry](bc-asn1-pkcs12test-indefinitelengthinputstream-stackoverflow-FIXED.md)
sibling doc), but this crash is a pure interpreter bug — `org/bouncycastle/`
never leaves the interpreter, so the JIT is not involved here.

## Evidence

```
CV=/data/data/wt-bc-suite-bench-20260709/target/release/cratonvm-bcbench-fix
JDK=/home/victor/jdk25
CP=apps/bc-java/core/build/classes/java/main:apps/bc-java/core/build/classes/java/test:apps/bc-java/core/build/resources/main:apps/bc-java/core/build/resources/test

$CV --java-home $JDK --stack-dump-on-timeout 0 --Xmx 1g -cp "$CP" \
  org.bouncycastle.asn1.test.RegressionTest
```

Output:

```
...
GeneralizedTime: Okay
BitString: Okay
Misc: Okay
Segmentation fault (core dumped)
```
`rc=139`. Deterministic across repeated runs.

### gdb backtrace

```
gdb -q -batch -ex run -ex "bt full" -ex "info registers" --args \
  $CV --java-home $JDK --stack-dump-on-timeout 0 --Xmx 1g -cp "$CP" \
  org.bouncycastle.asn1.test.RegressionTest
```

```
Thread 2 "main-vm" received signal SIGSEGV, Segmentation fault.
0x0000555556517b11 in cratonvm_vm::runtime::interpreter::array_descriptor_of ()
#0  array_descriptor_of ()
#1  execute_instruction ()
#2  execute_frame ()
#3  execute ()
#4  invoke_on_class_shared_inner ()
#5  invoke_shared ()
#6  cratonvm::run ()
```

(Release build, no debug symbols beyond function names — `#0` is `+17`
bytes into `array_descriptor_of`, i.e. very early in the function body,
consistent with the crash being in the first `shared.heap.kind_of(obj_ref)`
call rather than deeper in the `Reference`-element-type branch.)

Notable register state at the crash site:

```
r12   0x8000000000000000   -9223372036854775808   (i64::MIN)
```

`array_descriptor_of` (`vm/src/runtime/interpreter.rs:14278`) is called
from exactly one interpreter path relevant here — `checkcast` handling at
`vm/src/runtime/interpreter.rs:13525`:

```rust
Value::Object(Some(mut obj_ref)) => {
    ...
    let cast_ok = if let Some(src_desc) = array_descriptor_of(shared, obj_ref) {
```

`obj_ref` is only reachable here via a `Value::Object(Some(...))` match arm,
so it should be a well-formed `ObjectRef` by construction — the crash is
therefore either (a) a stale/dangling reference (object moved or freed by
GC without this local being updated — consistent with the wider "moving
young gen" / stale-reference-decode bug family already tracked for this
codebase), or (b) an unrelated register happening to hold `i64::MIN` and
not actually implicating `obj_ref` at all. Not yet distinguished.

## Next leads

- Reproduce standalone with a minimal `checkcast`-to-array-type snippet
  built from `X9Test`'s actual code path (`X9ECParameters` / point-encoding
  casts) to isolate which specific cast triggers it, rather than the full
  BC test class.
- Determine whether `obj_ref`'s value at the crash site is actually
  `0x8000...0000` (would point at a stale/sentinel-collision reference,
  matching the pattern already documented for the JIT deopt sentinel in
  `crypto/regression`-adjacent work) or whether `r12` is unrelated to
  `obj_ref` in this register allocation — needs a debug build or explicit
  `eprintln!` at `array_descriptor_of`'s entry to confirm.
- If it is a stale reference: check whether a GC safepoint can land between
  the `Value::Object(Some(mut obj_ref))` match and the `array_descriptor_of`
  call without `obj_ref` being treated as a live root during that window.
