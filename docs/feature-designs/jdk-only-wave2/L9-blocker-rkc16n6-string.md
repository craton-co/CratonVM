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
  `--dump-native-registry` on `probes/StringForcedNativeCanaryProbe`. So the
  2026-08-04 reachability fix did execute. `charAt`, `length` and `isEmpty`
  came back at **1 each** over a 120,000-iteration loop, because the JIT takes
  the loop and stops calling them.
* **...and then removing those natives made the same workload ~1.6x FASTER**
  (baseline 3800-6390 ms, this branch 2441-3568 ms, A-B-B-A interleaved, three
  rounds, identical checksums). Every native call pays the `safe_native_call`
  funnel; the bytecode gets compiled. The h2-bnf entries bought a slowdown and
  20 divergences.
* **`--jdk-only` census**: zero `native-shadows-bytecode` violations for
  `java/lang/String`, from 12; the registry goes from 80 `String` rows to 26.
* **The matrix moved 57 -> 37 divergences: 20 fixed, 0 regressed**, both modes
  byte-identical to each other. Two follow-up rounds took it to **8** — see
  *What is left* below for the per-step table. Every step is checked as a SET
  comparison, never a count: a count cannot see a row *worsen*.
* **`Compatible` byte-for-byte over `test_classes`**: 9 of 9 identical.

## What is left

The lane took the matrix from **57 -> 8** divergences in four steps, each with a
0-regression set comparison rather than a count:

| step | divergences | fixed | regressed |
|---|---:|---:|---:|
| drop the forced-native `String` policy | 57 -> 37 | 20 | 0 |
| UTF-16 `hashCode` + concat surrogates | 37 -> 21 | 16 | 0 |
| out-of-bounds class and message | 21 -> 8 | 13 | 0 |
| regex errors, US-ASCII decode | 8 -> 3 | 5 | 0 |

**The 3 that remain are one problem, and it is not a `String` defect**: HotSpot's
*helpful* `NullPointerException` messages ("Cannot invoke
\"String.isEmpty()\" because \"this.pattern\" is null") against our own
wording, on rows 241 / 259 / 275. That is `NullPointerException` message
synthesis for the whole VM -- it needs the bytecode operand that was null, which
is a interpreter/JIT feature, not anything `java/lang/String` does. Every
`String`-domain divergence this lane began with is closed.

## Three defects the removal surfaced

Taking a shadow off makes the shadowed code reachable, and two of the three
things underneath were broken. None of the three ended up re-masked — the
first looked like it had to be, and did not:

* `String.hashCode()` was **wrong for UTF-16 strings** — it hashed the backing
  BYTES sign-extended, not the code units. **Root-caused and FIXED 2026-08-05**
  ([record](../../internal/string-utf16-hashcode-reads-bytes-not-code-units-FIXED-20260805.md)).
  The defect was never in `String` or `StringUTF16` bytecode: it was the
  `ArraysSupport.vectorizedHashCode` **native**, which read one array slot per
  element for every `BasicType`. `StringUTF16.hashCode` calls it with `T_CHAR`
  over a **`byte[]`** of UTF-16 pairs, so it folded bytes where it owed code
  units. One shadow was hiding a second shadow. The `hashCode()` native this
  doc originally proposed to keep is therefore **dropped** — the bytecode is
  correct now, and keeping the native would have frozen the real bug in place
  where nothing reached it.
* `String.substring` out-of-range threw `ArrayIndexOutOfBoundsException`
  instead of `StringIndexOutOfBoundsException`. **The `String` half is FIXED
  2026-08-05**
  ([record](../../known-issues/preconditions-ignores-the-exception-formatter.md)
  — which supersedes the original `string-substring-bounds-…` record, that
  having named the wrong subsystem: the fault is `Preconditions` ignoring its
  exception-formatter argument, not anything `substring` does).

  Probing rather than reading found the worse half: for `charAt` the exception
  **class depended on the SIGN of the index** — `charAt(-1)` reached
  `Preconditions` and came back `ArrayIndexOutOfBoundsException` while
  `charAt(12)` came back `StringIndexOutOfBoundsException`. The matrix could not
  show that, because those rows already differed on message text.
  `String.checkIndex` is now a third F4 native, and every SIOOBE carries
  HotSpot's exact message. What is still open is the non-`String` half: NIO's
  callers of `Preconditions` still get `ArrayIndexOutOfBoundsException` where
  the JDK throws `IndexOutOfBoundsException`.
* `+` concatenation loses an unpaired surrogate. **FIXED 2026-08-05**
  ([record](../../internal/string-concat-loses-unpaired-surrogates-FIXED-20260805.md)).
  `execute_string_concat` accumulated into a Rust `String`, which cannot
  represent one. It now accumulates `Vec<u16>`. It was **three** loss points,
  not the one the record named — the argument, the folded recipe literal, and
  the `TAG_CONST` constant — and a probe written to separate them is the only
  reason it did not ship half-fixed with its own reproducer green.

## Residual

`resolve_step1_native` still passes `bytecode_available: false`. That is the
actual §1.4 hole, and it is why the lists could be inert while the natives kept
winning.

**Attempted and reverted 2026-08-05, with numbers** —
[record](../../known-issues/jdk-only/step1-bytecode-available-attempted-and-reverted.md).
Two of this paragraph's original claims turned out to be wrong: **4,796** is a
static count of registrations, not of dispatches (the observed change is 10 → 24
shadow observations on the matrix workload), and the default `--real-jdk` path
costs nothing because both consumers of the flag are `is_jdk_only()`-gated — the
392-case matrix stayed byte-identical.

It fails for a different reason than cost: step 1 runs BEFORE method resolution,
so it has the class name but not the resolved method, and `has_code` derived
from access flags on the named class is the wrong question when the hierarchy is
involved. `CharsetDecoder.decodeLoop` is abstract on the named class and
concrete on its subclasses; `--jdk-only` then dies with `AbstractMethodError:
... has no Code attribute`. The record proposes two restructurings and states
the acceptance test that this attempt failed.
