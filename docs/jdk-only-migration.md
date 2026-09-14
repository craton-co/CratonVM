# JDK-only mode — migration and operator guide

| | |
|---|---|
| **Status** | Stage 1 of 4 — **internal diagnostic**. `--jdk-only` may fail on programs that run fine under `--real-jdk`; that is the intended signal, not a bug in your program. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) |
| **Companions** | [`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md) · [`jdk-only-native-review.md`](jdk-only-native-review.md) · [`security/jdk-only-threat-model.md`](security/jdk-only-threat-model.md) · [`contributing/jdk-only-lane-operations.md`](contributing/jdk-only-lane-operations.md) |

## What the flag means

`--jdk-only` means **real class bytes are authoritative**:

- a real JDK runtime image is required — there is no silent fallback;
- no non-array class is fabricated;
- no `NativeKind::SyntheticStub` native is registered or invoked;
- concrete Java bytecode beats any registered native except a reviewed
  intrinsic;
- `ACC_NATIVE` methods bind to a bridge, or fail with a structured
  `MissingNative` error — never a stub.

Arrays, hidden classes, lambdas, proxies and reflection accessors are **allowed**
and carry their own distinct origin. They are not compatibility stubs. Likewise
the class-file `ACC_SYNTHETIC` flag in real `javac` output is normal and
untouched by this mode.

`--jdk-only` is a **runtime policy**, not a build feature. One binary runs both
modes, so you can A/B a failure in the same shell.

---

## Enabling it

```bash
cargo build --release -p cratonvm-cli

cat > /tmp/Hello.java <<'JAVA'
public final class Hello {
    public static void main(String[] args) {
        System.out.println("JDK-only mode");
    }
}
JAVA

"$JAVA_HOME/bin/javac" -d /tmp /tmp/Hello.java

target/release/cratonvm \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  -cp /tmp \
  Hello
```

On Windows the binary is `target\release\cratonvm.exe`; the flags are identical.

### Flag matrix

| Invocation | `JdkMode` | `CompatibilityMode` |
|---|---|---|
| *(no flag)* | `Real` | `Compatible` |
| `--real-jdk` | `Real` | `Compatible` |
| `--jdk-only` | `Real` | `JdkOnly` |
| `--synthetic-jdk` | `Synthetic` | `Compatible` |
| `--jdk-only --synthetic-jdk` | — | **configuration error** |

`--synthetic-jdk` additionally requires a build with the `synthetic-jdk` Cargo
feature; without it the launcher errors out at startup. `--jdk-only` needs no
special build.

### Diagnostic capture

```bash
target/release/cratonvm \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  --jdk-only-report              /tmp/cratonvm-jdk-only.json \
  --dump-native-registry         /tmp/cratonvm-natives.json \
  --dump-class-origins           /tmp/cratonvm-classes.json \
  --dump-missing-natives-grouped /tmp/cratonvm-missing.json \
  -cp app.jar \
  com.example.Main
```

Add `--trace-jdk-only` to log each violation as it happens, and
`--explain-jdk-only` for the long-form explanation per violation. **Absolute
paths are redacted from reports unless `--explain-jdk-only` is passed** — check
before attaching a report to a public issue, and check again if you passed it.

`--dump-native-registry` and `--dump-missing-natives[-grouped]` work on any
current build. The other three flags arrive with wave 1.

---

## Reading a failure

A violation names the class, method, descriptor, class origin, attempted native
kind, the JDK feature version, and the fallback. Example shape:

```text
CratonVM JDK-only violation: compatibility implementation required

Requested class:
  java/util/function/Function$Identity

Requested from:
  java/util/function/Function.identity()Ljava/util/function/Function;

Reason:
  No class bytes were found, and the compatible runtime would create a
  synthetic stand-in.

JDK:
  feature version: 25
  java.home: <redacted>
  module: java.base

Remediation:
  This indicates an incomplete lambda/metafactory runtime service.
  Re-run with --real-jdk to use compatibility mode, or collect:
    --jdk-only-report jdk-only-report.json
```

### Violation kinds and what each one means

The `kind` tag is stable and greppable (`JdkOnlyViolation::kind()`).

| Kind | It means | Your next step |
|---|---|---|
| `compatibility-class-requested` | The VM was about to fabricate a class with no real bytes. | If it is a **JDK** class: a runtime service is incomplete — file it. If it is an **application or dependency** class: your classpath is genuinely missing a jar. Fix the classpath; the compatible mode was hiding a real error. |
| `synthetic-native-registered` | A `SyntheticStub` registration was refused at VM init. | Nothing you can do at the command line. The named registration site is the work item. Many of these are permanent bridges carrying the wrong tag — see [`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md). |
| `synthetic-native-invocation` | A `SyntheticStub` was reached at dispatch time. | A dispatch path bypassed the registration gate. Always worth filing: it names a hole in the resolver, not just in one native. |
| `missing-native` | An `ACC_NATIVE` method has no registered bridge. | The most actionable kind. Attach `--dump-missing-natives-grouped` — the module grouping tells you which JDK module is under-served. |
| `native-shadows-bytecode` | A registered native was about to win over concrete real bytecode. | Under stage-1 policy this is recorded, not enforced. It is the leading indicator for a wrong-result bug. |
| `missing-boot-class` | A class expected in the JDK runtime image was not found. | Almost always a `--java-home` / JDK-layout problem. Check the searched paths printed in the error. |
| `missing-implementation` | Neither bytecode nor a native exists for the method. | An abstract/absent method reached dispatch. File with the reproducer. |

### The two-run triage

Always run both modes before concluding anything:

```bash
# 1. Strict.
target/release/cratonvm --jdk-only  --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# 2. Compatible fallback — same binary, same everything else.
target/release/cratonvm --real-jdk  --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# 3. HotSpot, for ground truth.
"$JAVA_HOME/bin/java" -cp app.jar com.example.Main
```

Interpretation:

| Strict | Compatible | HotSpot | Reading |
|---|---|---|---|
| fail | pass | pass | A compatibility substitution is load-bearing. This is the case the mode exists to find — **file it**. |
| fail | fail | pass | An ordinary CratonVM bug, unrelated to strictness. File it as a normal bug, not a JDK-only issue. |
| fail | fail | fail | Your program or classpath. Not a VM issue. |
| pass | pass | fail | Divergence from HotSpot in *our* favour — still a divergence. Worth reporting. |
| pass | fail | pass | Surprising; attach both reports. |

A timeout with no result line is frequently a crash, not a hang. Capture stderr.

---

## Falling back

`--real-jdk` is the supported opt-out and stays the default. It is not going
away during this rollout.

```bash
# Strict.
cratonvm --jdk-only    --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# Opt out, still a real JDK. This is the fallback.
cratonvm --real-jdk    --java-home "$JAVA_HOME" -cp app.jar com.example.Main

# Standalone synthetic library, when compiled in. A different thing entirely —
# not a fallback for strict mode, and it conflicts with --jdk-only.
cratonvm --synthetic-jdk -cp app.jar com.example.Main
```

The harness rule: a strict CI job must pass `--jdk-only`, must assert the mode
it actually ran in, and must **never** silently retry under `--real-jdk`. An
implicit retry converts a strict regression into a green build.

---

## `CRATONVM_REAL=-stubs` is deprecated as a user-facing mode

```bash
# Existing low-level approximation. Still works; no longer the recommended
# interface. Prints a one-time note pointing at --jdk-only.
CRATONVM_REAL=-stubs cratonvm --real-jdk -cp app.jar com.example.Main

# Recommended.
cratonvm --jdk-only -cp app.jar com.example.Main
```

`CRATONVM_REAL=-stubs` expands to `CRATONVM_NO_STUBS` (declared in
`types/src/flag_groups.rs`) and remains supported **as a native-registry
filter**. It is being deprecated as a *mode* because it can only express one
third of the contract:

| Contract half | `CRATONVM_REAL=-stubs` | `--jdk-only` |
|---|---|---|
| Drop registered `SyntheticStub` natives | yes | yes, plus provenance for each refusal |
| Refuse fabricated classes | **no** | yes |
| Make concrete bytecode authoritative at dispatch | **no** | yes (wave 2 enforcement) |
| Require a real boot image with a named error | no | yes |
| Structured, machine-readable violations | no | yes |

`CRATONVM_DBG=dropped-stubs` (→ `CRATONVM_DBG_DROPPED_STUBS`) still surfaces the
registrations that policy discarded, and remains useful alongside either.

**Deprecation stance:** no removal date. The env token keeps working for as long
as the registry filter exists. What changes is the recommendation and the
one-time note — do not build new tooling on the env token.

---

## Filing an actionable issue

Use [`.github/ISSUE_TEMPLATE/jdk-only.yml`](../.github/ISSUE_TEMPLATE/jdk-only.yml).
An issue is actionable when it carries all five of:

1. **JDK** — vendor, feature version, `java -version` banner, and the
   `--java-home` you passed.
2. **Platform** — OS and architecture. Windows/Linux differences are real in the
   I/O, process and networking families.
3. **The report** — `--jdk-only-report` output, plus `--dump-class-origins`
   and/or `--dump-missing-natives-grouped` when the violation kind points at
   them. Confirm the redaction state before attaching.
4. **A reproducer** — minimal Java source and the exact command line, ideally
   compiled with `javac --release <N>`. A jar we cannot run is not a reproducer.
5. **The `--real-jdk` result** — did the same command pass under the fallback?
   This one line decides whether the issue is "a compatibility substitution is
   load-bearing" or "an ordinary bug", and routes it to a completely different
   owner.

Do not file: a `synthetic-native-registered` violation on its own. Those are
already enumerated by the census and tracked in
[`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md). File the
*program* that fails because of one.

---

## Rollout stages

The default does **not** change from real-compatible to JDK-only during this
rollout. The current deterministic real-JDK default is itself a compatibility
contract.

| Stage | User contract | Gate to enter |
|---|---|---|
| **1 — Internal diagnostic** *(current)* | `--jdk-only` may fail. The census and the errors are the product, not successful execution. | Flag exists; violations are structured; no synthetic-stub *invocation* goes unrecorded. Failures are expected. |
| **2 — Experimental** | The core Java corpus and selected frameworks pass. The fallback is documented and works. | Linux + JDK 21 strict job **blocking**. Zero `CompatibilityStub` classes on the core corpus. Startup/memory budgets published (see [`benchmarks/jdk-only.md`](benchmarking/jdk-only.md)). |
| **3 — Preview** | JDK 21 and 25, Linux and Windows, broad runtime-service coverage. | Differential and regression gates blocking in strict mode. No new unapproved HotSpot divergence. Windows filesystem/process/networking vectors stable. |
| **4 — Stable** | A declared JDK/platform matrix, published performance budgets, a support policy. | No known P0/P1 compatibility substitution remains open in [`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md). Zero final `SyntheticStub` registrations. Zero synthetic-stub invocations across interpreter, JIT, JNI, reflection and method-handle paths. |

Between stages, three things stay constant: `--real-jdk` remains the default,
the `synthetic-jdk` build configuration keeps its blocking compile/test job so
the fallback code does not rot, and strict CI never retries under the fallback.
