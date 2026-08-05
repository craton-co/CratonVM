# L9 — Blocker: real `java/lang/String` bytecode during JDK `<clinit>` — **CLOSED 2026-08-04**

**Owns:** `vm/src/runtime/interpreter/` (resolution path)
**Gated on:** nothing — it gated L11, which is now unblocked *and* done for
item 3.
**Effort:** L as scoped. The work that closed it was a different shape; see
below.
**Outcome:** [`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`](../../internal/forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md)

## Closed, and the premise was false

This lane's deliverable was **"make real `String` bytecode work during JDK
`<clinit>`s"**. Real `String` bytecode already worked. Two separate findings:

1. **RKC16N.6 does not reproduce.** The April-2026 symptom
   (`NoSuchMethodError: java/lang/String.charAt(I)C` during
   `StandardCharsets.<clinit>`) was a *partial class load*, exactly as its
   sibling record RKC16N.7 guessed — and the cause was fixed in April by
   **RKC16N.9**, a jimage header version-decoding bug in `reader/src/jimage.rs`
   (a `u32` read as two `u16`s, inverted on little-endian disk) plus a missing
   `lib/modules` fallback in `discover_boot_classpath`. Nobody went back to the
   `String` workaround, so its comment kept asserting a live defect for four
   months, and three lanes were planned around it.

2. **The lists were inert.** Step 3 of this doc said to confirm that "by
   removing them and booting, not by argument". Done, on a throwaway binary,
   before anything else: with both lists deleted the 392-case `String` matrix
   was **byte-identical** in both modes, all 38 exercised `java/lang/String`
   registry slots reported **identical invocation counts**, and boot was clean.
   Not one line moved and not one counter moved.

   `resolve_step1_native` (`try_stackless_invoke` step 1) resolves the triple in
   the registry and dispatches whatever it finds *before* either list runs, and
   has no list of its own. Registration was always the gate.

So the deliverable was not a resolution fix. It was: delete four copies of a
dead policy, and put the decision where it was actually being made.

## What the natives were doing while the lists watched them

`probes/StringPolicyMatrixProbe` (392 cases, HotSpot 25 control) says the
forced natives were the **cause** of 57 divergences: unpaired surrogates
decoded to U+FFFD, nine shapes returning a default where HotSpot throws
(`matches("[")` answering `false`; `new String(bytes, "NO-SUCH")` silently
decoding as UTF-8), `"US-ASCII"` decoding as Latin-1, every
`StringIndexOutOfBoundsException` carrying a null message, and
`s.substring(0) == s` false.

The five h2-bnf shapes were justified as *"trivially equivalent to the real-JDK
bytecode for every input"*. That is true of the algorithm and false of the
implementation, in three separate ways, and no test asked.

## Verification, as this doc specified it

* **Strict boot with both lists removed** — done first, on its own binary. This
  is what produced the inertness finding.
* **Both probes vs HotSpot in both modes** — `JdkOnlyCensusLoadProbe` 9/9 and
  `JdkOnlyBreadthProbe` 15/15, three runs each per mode, exit status checked.
  The one residual is `DecimalFormat` grouping in the breadth probe, present in
  **both** CratonVM modes and therefore not a strict-mode defect.
* **The h2-bnf fix confirmed with a counter**, not by reading the code:
  `startsWith` 2,881 invocations, `substring(I)` 173, via
  `--dump-native-registry` on `probes/StringForcedNativeCanaryProbe`. Worth
  recording that `charAt`, `length` and `isEmpty` came back at **1 each** over a
  120,000-iteration loop — the JIT bypasses them once the loop compiles, so
  those entries' real footprint is far smaller than their record implies.
* **`--jdk-only` census**: zero `native-shadows-bytecode` violations for
  `java/lang/String`, from 12.

## Residual

`resolve_step1_native` still passes `bytecode_available: false`. That is the
actual §1.4 hole, and it is why the lists could be inert while the natives kept
winning. Closing it sends **4,796** shadowing `Bridge` registrations to the
bytecode under `--jdk-only` at once — far outside this lane, and precisely why
the fix here is scoped to one class at the registration boundary. It belongs
with L11 / L12 §11 and needs its own measurement.
