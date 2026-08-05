# `HazelcastAutoConfigurationClientTests`: a JIT dispatch defect, not a schema or classpath problem

**Status: OPEN — found 2026-08-05, re-diagnosed 2026-08-05.**

The original page filed this as a schema-version mismatch and offered two
candidate causes — a classpath resource-resolution bug and a stale `~/.m2`
artifact. **Both are refuted below, as is the schema-mismatch reading itself.**
The failure is a JIT dispatch defect; the XML is a bystander.

## Reproducer: one second, no Spring, no JUnit

`probes/HzConfigLoadProbe.java` — just `Config.load()`, the call `@BeforeAll`
makes:

```bash
CP="/tmp/hzprobe:$(cat <sb>/module/spring-boot-hazelcast/build/cratonvm-test-cp.txt)"
<craton> --java-home <jdk25> -cp "$CP" HzConfigLoadProbe 5
```

Prints `HZLOAD-SUMMARY: ok=<n> fail=<n>`. Deterministic on Azure Linux:

| arm | result |
|---|---|
| HotSpot | ok |
| craton `--nojit` | ok |
| craton | **fail 5/5** |

**Linux only.** The same binary on Windows passes 3/3, and the full test class
passes 12/12 there. Every measurement below is Azure Linux.

## What it is not

**Not `@ClassPathOverrides` / `~/.m2`.** `HazelcastAutoConfigurationClientTests`
carries no such annotation and has no annotated superclass, so it resolves
against the Gradle test classpath. The standing Maven-cache guidance does not
apply to it.

**Not resource shadowing.** The module's test classpath carries exactly one
`hazelcast` jar (5.5.0) and one `hazelcast-spring`. There is no second
`hazelcast-config-*.xsd` to pick up by mistake.

**Not the document the page named.** The error is in the *server*
`schema/config` namespace; the test's own fixtures are 16–18 line *client*
configs in `schema/client-config`. The failing document is Hazelcast's bundled
`hazelcast-default.xml`, loaded by `Config.load()` — the log says so
(`Loading 'hazelcast-default.xml' from the classpath`).

**Not a parse/DOM problem.** Hazelcast validates a `DOMSource`
(`AbstractXmlConfigHelper.schemaValidation`). `probes/XmlDomShapeProbe.java`
dumps that DOM: CratonVM and HotSpot agree exactly — 162 elements, max depth 5,
`kubernetes` correctly nested at `hazelcast/network/join/kubernetes`, identical
path hash. The tree handed to the validator is right.

**Not GC.** `--Xmx 256m`, default, and `--Xmx 8g` all fail 5/5.

**Not a miscompile of the method it fingerprints to.**
`probes/AddAttrNsProbe.java` drives the REAL
`XMLAttributesImpl.addAttributeNS` (not a replica) for 20 000 rounds across the
array-growth boundary: CratonVM and HotSpot produce the identical checksum with
zero length or read-back mismatches.

## What it is

A **JIT dispatch defect on the single-pass direct-call / inline-cache edge**.

| lever | result |
|---|---|
| baseline | fail 5/5 |
| `CRATONVM_JIT_SP_INLINE_IC=0` | **ok 5/5** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | **ok 5/5** |
| `CRATONVM_JIT_IR_DIRECT_CALL=0` | fail 5/5 |

The master switch and the single-pass inline cache each clear it; the IR-only
opt-out does not. So the bad edge is emitted by the single-pass backend, not by
the optimizing pipeline's direct-call lowering.

It is an **interaction**, not one bad method:

| allow-list / deny | result |
|---|---|
| only `XMLAttributesImpl` compiles | ok 3/3 |
| only `xerces/` compiles | fail 3/3 |
| deny `XMLAttributesImpl.addAttributeNS` | ok 5/5 |

Compiling `addAttributeNS` is necessary but not sufficient — a broader set of
compiled xerces methods has to be present too. Denying the caller side is only
partially effective (`jaxp/` and `jaxp/validation/` each 1 ok / 2 fail), which
is the signature of a pressure-dependent collision rather than one statically
mis-bound site.

### Relationship to the recycled-`JitInvokeInfo` family — NOT the same fix

This is the same *shape* as `383e7f5cf` (*a recycled `JitInvokeInfo` address let
one call site serve another's dispatch*) and its follow-up `9d0636e73`, but
**neither fixes it**. Measured on a binary carrying `383e7f5cf`, `836631dcc`
and `9d0636e73`: still fail 5/5. Nor do the consumers that family runs through:

| lever | result |
|---|---|
| `CRATONVM_JIT_NO_NATIVE_SITE_CACHE=1` | fail 5/5 |
| `CRATONVM_JIT_METHOD_SITE_CACHE=0` | fail 5/5 |
| `CRATONVM_JIT_FIELD_SITE_CACHE=0` | fail 5/5 |
| `CRATONVM_JIT_LEAK_CODE=1` / `FREE_CODE=0` / `POISON_FREE=1` | fail 5/5 |

So it is not the native-site-cache consumer, and it is not a stale code address
from a freed/recycled code buffer. It is a distinct residual on the same edge.

One symptom detail worth keeping: the offending element is **not stable**. The
same fixture reports `kubernetes` on one binary and `max-size` on another —
whatever the mis-dispatched call returns decides which element the content
model rejects. Any future report of "Hazelcast config fails schema validation
on element X" is probably this, whatever X is.

## A trap this cost a cycle, recorded so the next reader skips it

`CRATONVM_JIT_NO_DUP_X1=1` turns the failure green, which looks like it
implicates the `dup_x1` codegen arm — `addAttributeNS`'s only unusual construct
is the `fLength++ == fAttributes.length` javac emits as `dup_x1`. It does not.
That arm calls `self.fail(...)` when the flag is set, so the flag merely stops
the method compiling, exactly like `CRATONVM_JIT_DENY` on it. Both `dup_x1`
implementations (single-pass `x64/bytecode_walk.rs`, IR `ir.rs`) were read
against JVMS and are correct, and `CRATONVM_DBG_DUPX_TRACE=1` confirms the
rotate does the right thing in this very method:

```
before=[Frame(112), Frame(120), Frame(128)] marks=[true, false, false]
after =[Frame(128), Frame(112), Frame(120)] marks=[false, true, false]
```

`CRATONVM_JIT_DUPX_EAGER_CANON=1` also fails, which by that flag's own
documentation would point at the rotate model — another reason the `dup_x1`
reading looked plausible. It is still wrong.

## Where to look next

The edge is `direct_jit_callee_calls_enabled`'s single-pass consumers: the
eager callee-compile-and-direct-`CALL` in `try_compile_inner`, and the inline
virtual MIC fast path in `x64.rs`. The question to answer first is *which* call
site binds the wrong target — the caller-side denies above narrow it to the
xerces validation graph but not to one method, so a per-site trace of what the
single-pass backend binds (class, method, descriptor, entry) at each direct
call, diffed between a passing and a failing configuration, is the next
instrument to build.

Note that the sibling fix for the `383e7f5cf` family found a **by-NAME** class
resolution (`resolve_native_owner_for_receiver` walking superclasses by name,
which two loaders make ambiguous). Worth checking whether the single-pass
direct-call binding resolves its target the same way.

## Affected classes

- `module/spring-boot-hazelcast` — `HazelcastAutoConfigurationClientTests`
  (12 tests, all fail: `@BeforeAll` throws, so the container fails with
  `tests=0 containersFailed=1`). HotSpot 12/12, `--nojit` 12/12.
  Regression: the 2026-08-02 full suite scored this class PASS 12/12 in 43.0s;
  2026-08-05 scored FAIL `tests=0` in 2.6s.
