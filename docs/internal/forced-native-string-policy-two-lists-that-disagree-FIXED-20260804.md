# The forced-native `java/lang/String` policy — FIXED 2026-08-04

**Status:** FIXED on `fix/rkc16n6-string-clinit-20260804`. Filed 2026-07-31 as
`docs/known-issues/jdk-only/forced-native-string-policy-two-lists-that-disagree.md`;
materially reduced 2026-08-04 (the dead h2-bnf block was made reachable), and
closed the same day by the work recorded here.

This also closes **L9**
([`L9-blocker-rkc16n6-string.md`](../feature-designs/jdk-only-wave2/L9-blocker-rkc16n6-string.md))
and **item 3 of L11** — but not in the shape either doc expected. Read *The
premise was false* first; the rest only makes sense after it.

## The premise was false, in two separate ways

### 1. RKC16N.6 does not reproduce. Nothing fails.

L9's deliverable was "make real `String` bytecode work during JDK `<clinit>`s",
on the strength of a Session-94 (April 2026) comment saying real-JDK
`java/lang/String` bytecode resolution *"is failing for these basic methods
during JDK class clinits like `java/nio/charset/StandardCharsets.<clinit>`"*.

It is not failing. The original symptom was
`NoSuchMethodError: java/lang/String.charAt(I)C`, and its sibling record
RKC16N.7 guessed the cause: *"JDK 25 `String` from `lib/modules` is loaded as a
partial class"*. That guess was right, and the cause was fixed in April by
**RKC16N.9** — a jimage header version-decoding bug in `reader/src/jimage.rs`
(a `u32` read as two `u16`s, inverted on little-endian disk) plus a missing
`lib/modules` fallback in `discover_boot_classpath`. Once the image parsed, the
methods were there. Nobody went back to the `String` workaround, so its comment
kept asserting a live defect for four months and two lanes were planned around
it.

### 2. The lists were inert. Deleting them changes nothing.

Not "small", not "mostly" — **nothing**. Measured, not argued, because L9 asked
for exactly this experiment (*"confirm the two lists are no longer load-bearing
by removing them and booting, not by argument"*):

| | baseline binary | both lists deleted |
|---|---|---|
| `StringPolicyMatrixProbe`, `--real-jdk` (392 cases) | 57 diverge from HotSpot | **57, the same 57** |
| `StringPolicyMatrixProbe`, `--jdk-only` | byte-identical to `--real-jdk` | byte-identical |
| `java/lang/String` registry slots with a non-zero invocation count | 38 | **38, identical counts** |
| boot | clean | clean |

Not one line of the transcript moved and not one counter moved.

**Why.** `resolve_step1_native` — `try_stackless_invoke` step 1 — resolves the
`(class, method, descriptor)` triple in the native registry and dispatches
whatever it finds **before** either list is consulted, and it has no list of
its own. It passes `bytecode_available: false` to section 7 (a marked wave-2
gap: it runs before method resolution and genuinely does not know), so a
`Bridge` wins there in both modes. Every `String` native the lists nominally
governed was already winning by registration alone.

So all three documented copies of the policy — and a **fourth** nobody had
counted — were dead code that read exactly like live policy. Which is worse
than the divergence the record was filed about: the two halves were being kept
carefully in sync, by a test, with a 29-shape verdict table, over a decision
neither of them was making.

### The fourth copy

`is_jdk_string_charset_name_constructor_override` forced
`String.<init>([BLjava/lang/String;)V` and `([BIILjava/lang/String;)V` on both
the cold and warm paths. It is a differently-named predicate rather than a
`String` list, which is why a grep for the 21 names did not find it. Deleted
too.

## What the natives were actually doing: 57 divergences from HotSpot

`probes/StringPolicyMatrixProbe` is 392 cases over a corpus built for the
inputs where a layout-neutral native and real bytecode are most likely to
differ. Run on HotSpot 25 first, then on both CratonVM modes. The forced
natives are the **cause** of every divergence below; none of them is a
`String`-bytecode defect.

**Unpaired surrogates are destroyed.** The natives read a `String` through
`read_string` (UTF-8) and write it back through `create_string`, and an
unpaired surrogate is not a Unicode scalar value, so it cannot survive:

```
charAt*(LONE)          HotSpot  "0078 D801 0079"    CratonVM "0078 FFFD 0079"
substring2(SUPP,0,3)   HotSpot  "ab\uD801"          CratonVM "ab�"
concat(SUPP,LONE)      HotSpot  "ab𐐁cdx\uD801y"
                       CratonVM "ab𐐁cdx�y"
```

`String::from_utf16_lossy` in `native_string_substring_one` is one of the
sites. It also breaks the paired properties: splitting a surrogate pair and
rejoining the halves no longer reconstitutes the string.

**Exceptions become values.** Nine shapes returned a plausible default where
HotSpot throws:

```
startsWith(PLAIN,null)        HotSpot NPE                        CratonVM false
contains(PLAIN,null)          HotSpot NPE                        CratonVM false
indexOf(PLAIN,(String)null)   HotSpot NPE                        CratonVM -1
replaceCS(PLAIN,null,"x")     HotSpot NPE                        CratonVM null
matches(PLAIN,"[")            HotSpot PatternSyntaxException     CratonVM false
replaceAll(PLAIN,"[","x")     HotSpot PatternSyntaxException     CratonVM input unchanged
replaceAll-bad-group ($9)     HotSpot IndexOutOfBoundsException  CratonVM "bc"
new String(utf8,"NO-SUCH")    HotSpot UnsupportedEncodingException
                              CratonVM a UTF-8 decode
new String(utf8,-1,2,"UTF-8") HotSpot StringIndexOutOfBounds     CratonVM "\u0000H"
```

`matches("[")` returning `false` is the worst shape here: the fast path's
*error* branch fell through to comparing the subject against the pattern
**text**, so an invalid regex produced a confident wrong verdict.

**Wrong charset.** `new String(latin1Bytes, "US-ASCII")` decoded as Latin-1,
returning `"éaÿ"` where HotSpot returns `"�a�"`.

**No exception messages.** Every `StringIndexOutOfBoundsException` from the
natives carried `msg=null` against HotSpot's `"Index -1 out of bounds for
length 12"` / `"Range [3, 2) out of bounds for length 12"`.

**Lost identity.** `s.substring(0) == s` and `s.concat("") == s` are `true` on
HotSpot and were `false` here.

The five h2-bnf shapes were added with the justification that they have
*"straightforward, locale/Unicode-independent semantics ... trivially equivalent
to the real-JDK bytecode for every input"*. That is true of the algorithm and
false of the implementation, in three separate ways, and no test asked.

## The fix

**One decision, at registration, for every dispatch path.**
`NativeMethodRegistry::register` now drops every `java/lang/String` `Bridge` in
real-JDK mode. A dropped registration is invisible to `check_override`, to
`force_native_over_real_jdk_bytecode`, to `resolve_step1_native` and to the
JIT's direct-bind ladder alike — which is what "the lists are replaced by
nothing" has to mean when the lists were never the gate.

The rule is a **kind**, not another method list, and that is the adjudication
itself: against the JDK 25 image, 79 of the 80 `java/lang/String` registrations
target a method declared with a `Code` attribute (section 1.4
`NativeShadowsBytecode`), and exactly one — `intern()` — is genuinely
`ACC_NATIVE` and therefore a legitimate section 1.5 bridge.

**Deleted:**

* `check_override`'s 21-name descriptor-blind arm and
  `cold_forced_native_string_name` (`vm/src/vm/vm_exec.rs`);
* the warm-path exclusion, `warm_forced_native_string_candidate`, the
  `substring(II)` block, the h2-bnf five-shape block and the SBR-02 arm
  (`vm/src/runtime/interpreter/native_override.rs`);
* `is_jdk_string_charset_name_constructor_override` and both its call sites —
  the fourth copy;
* the JIT's `String.toLowerCase(Ljava/util/Locale;)` direct bind,
  `STRING_LOCALE_LOWER_DIRECT_FN`, its setter and its VM-side helper
  (`jit/src/lib.rs`, `vm/src/jit/helpers.rs`).

The `StringLatin1.toLowerCase` bind **stays**: that is the helper the real
`String.toLowerCase(Locale)` bytecode delegates to, so it accelerates the real
path instead of replacing it, and its input is Latin-1 by construction and
cannot reach the surrogate cases.

**Kept, as reviewed `NativeKind::Intrinsic`** — section 1.4's own answer, taken
on kind with no name list anywhere:

* the four `CRATONVM_NATIVE_STRING_REGEX` shapes (`replaceAll`,
  `replaceFirst`, `matches`, `replace(CharSequence,CharSequence)`). They are
  ~2x faster than HotSpot on the `PluginXmlParser.format()` shape they were
  written for (`probes/StringRegexCostProbe`: 47-64 ms vs HotSpot's 100 ms,
  identical digests), which is what an intrinsic is for;
* `hashCode()`, and this one is **not** a performance argument — see the next
  section.

They are also the **first callers of `register_with_kind`**, so the census's
`kind_stated` column now records that somebody adjudicated these four rather
than that they inherited an ambient `set_category`. That was step 2 of the
[ambient-`NativeKind` record](../known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md),
which had the entry point and zero callers.

**Repaired before promoting them.** Being fast is not a review. The four now
raise what the JDK raises:

* an uncompilable pattern is a `PatternSyntaxException` — a new
  `RuntimeError` variant mapping to `java/util/regex/PatternSyntaxException`,
  the concrete class rather than its `IllegalArgumentException` parent,
  because validation code catches it by name. This also fixes every other
  `compile_java_regex` caller, `Pattern.compile` included;
* a `null` regex / replacement / target is an NPE instead of a null `String`.

## CORRECTION, 2026-08-05: two of the three were regressions, not discoveries

The section below said three defects were *surfaced* by removing the shadows.
**Two of them were caused by it**, and this record said otherwise for a day.
Both were found by auditing the change, not by any failure report -- and both
fail silently, so no report was coming.

The drop removes every `java/lang/String` `Bridge` in real-JDK mode. That rule
is right. Its exemptions were derived from the registrations *I went looking
for*, instead of from the registrations it actually drops, and four of those
carried a comment at their own site stating exactly what breaks without them:

* **F4** -- `checkBoundsBeginEnd` / `checkBoundsOffCount`. The generic
  `Preconditions.checkFromToIndex(int,int,int,BiFunction)` override discards
  its `SIOOBE_FORMATTER` and always raises `ArrayIndexOutOfBoundsException`;
  F4 intercepts the two String helpers so String-domain callers get
  `StringIndexOutOfBoundsException`. Dropping them put
  `"Hello, World".substring(-1)` back on the wrong class, which
  `catch (StringIndexOutOfBoundsException)` does not catch. Filed as
  [`preconditions-ignores-the-exception-formatter.md`](../known-issues/preconditions-ignores-the-exception-formatter.md),
  which supersedes the earlier, wrong record.
* **DF05** -- `String(StringBuilder)` and `String(AbstractStringBuilder, Void)`.
  The real ctor does `Arrays.copyOfRange` over the builder's `byte[]`; this
  VM's builders are `char[]`-backed. A builder holding seven characters came
  back as four, every second byte the zero high half of a Latin-1 char.
  **Silent content corruption on `new String(sb)`, no exception anywhere.**

All four are `register_with_kind(.., Intrinsic)` now, each site saying the kind
is load-bearing and why. The remaining two flagged registrations were tested
and stay dropped, because their claims no longer reproduce: `String(byte[])`
("silently complete with zero value bytes") and `indexOf(String,int)` plus its
static helper ("the helper returns 0 every iteration"). A comment is a claim,
in both directions.

### What the earlier reasoning got wrong

For F4 it argued from a control -- `charAt(-1)` still produced the right class,
therefore the fault was `substring`'s bounds check specifically, therefore
pre-existing. The control was sound; the inference was not. It established
*where* the difference was and was read as establishing *when* it appeared. One
grep of the registry for the triple would have shown a 40-line comment
describing the exact failure mode.

### The measurement could not have caught it either

The 392-case matrix reported **37 divergences before and after** the F4 fix.
Those eight rows were already unequal on their message text, so a change of
exception *class* -- the half that changes control flow -- moved nothing the
count could see. **A row can get materially worse while staying "diverging".**
DF05 was worse still: `new String(sb)` is not in the matrix at all.

`probes/StringDroppedNativesProbe` is what found both, and it exists because the
audit asked "what does this drop remove?" instead of "what do I remember?". It
is 29/29 identical to HotSpot 25.

### The guard

`the_surviving_string_registration_set_is_exactly_this` pins the exact set of
surviving `java/lang/String` registrations, two-sided, generated from a real
`--dump-native-registry` run rather than written from memory. The two one-sided
tests written with the original change both passed while four registrations
vanished. Verified by injecting the precise mistake: changing one
`register_with_kind(.., Intrinsic)` back to `register` fails it, naming
`checkBoundsBeginEnd`.

## Removing the natives surfaced one defect it was hiding, and two it broke

Dropping a shadow makes the shadowed code reachable, and two of the three
things underneath it were broken. This is the honest cost of the change and it
is why the divergence count went from 57 to 37 rather than to zero.

**1. `String.hashCode()` is wrong for UTF-16 strings** —
[FIXED 2026-08-05](string-utf16-hashcode-reads-bytes-not-code-units-FIXED-20260805.md).
The bytecode hashes the first `length()` BYTES of the backing array, each
sign-extended to a `char`, instead of the `length()` code units:
`"ΣΟΣ".hashCode()` is `62956255` where the JLS and HotSpot say
`924359`. The object itself is fine — `length`, `charAt`, `toCharArray` and
`equals` on it all agree with HotSpot, which is what localises the fault to
`hashCode`'s dispatch target. Solving the observed hashes for their input
sequence gives that exact reading for all four probe strings, sign extension
included (`0xA3` hashed as `0xFFA3`).

**So `String.hashCode()` was registered `Intrinsic` for CORRECTNESS** — letting
the bytecode win would have replaced a right answer with a wrong one for every
non-ASCII `String` key in the VM.

**That registration is now withdrawn (2026-08-05), and the paragraph above was
wrong about where the defect lived.** It is not a `StringUTF16` bytecode bug:
`StringUTF16.hashCode` delegates to `ArraysSupport.vectorizedHashCode`, and the
*native override* of that helper read one array slot per element for every
`BasicType`. `T_CHAR` over a `byte[]` is two slots. So one shadow was hiding a
second shadow, and the "the bytecode under it is broken" reasoning that
justified keeping this native was itself a consequence of a different native.
With the helper fixed the bytecode is correct, and the re-measurement this
paragraph asked for came back a wash overall and **2-4x slower on the cached
read** — so the registration was deleted rather than kept.

**2. `String.substring` out-of-range throws `ArrayIndexOutOfBoundsException`** —
**superseded; see the correction above.** This was a regression of this change,
not a pre-existing defect, and it is fixed. The underlying `Preconditions`
defect it exposed is real and open:
[filed](../known-issues/preconditions-ignores-the-exception-formatter.md).
`charAt(-1)` is the control: its bytecode reaches the right class, so this is
`substring`'s bounds check specifically. The two are siblings under
`IndexOutOfBoundsException`, so `catch (IndexOutOfBoundsException)` is
unaffected but `catch (StringIndexOutOfBoundsException)` is not.

This one was **not** re-masked, deliberately: the native's version of those six
rows was already wrong (right class, `msg=null`), and keeping it would have
cost the four rows the bytecode fixes — including `substring` splitting a
surrogate pair, which the native turned into U+FFFD. Silent data corruption on
a valid input is worse than a loud exception of the wrong class. The trade is
stated here rather than hidden.

**3. `+` concatenation loses an unpaired surrogate to U+FFFD** —
[FIXED 2026-08-05](string-concat-loses-unpaired-surrogates-FIXED-20260805.md). Found by
accident: `probes/StringUtf16HashProbe` builds its lone-surrogate string with
`new String(char[])` and it survives, while
`probes/StringPolicyMatrixProbe` builds the same string with `+` and it does
not. That difference is what proves the residual `LONE` rows are a concat
defect and not a `charAt` / `trim` / `hashCode` defect — a distinction the
matrix alone would have got wrong.

## The h2-bnf natives were not just wrong, they were SLOWER

The record these entries came from argues them as a measured performance fix.
Re-measured on the workload they were written for
(`probes/StringForcedNativeCanaryProbe`, 4,000 iterations, A-B-B-A interleaved,
three rounds, identical checksums on every run):

| arm | ms |
|---|---|
| baseline (natives forced) | 3800, 4027, 4112, 4716, 5447, 6390 |
| this branch (real bytecode) | 2441, 2443, 2485, 2504, 2868, 3568 |

Removing them made the h2-bnf scan **~1.6x faster**. That is not a paradox:
every native call pays the `safe_native_call` funnel, which is the per-call
floor for this VM, while the real bytecode gets JIT-compiled and inlined. The
canary census says the same thing from the other side — over a
120,000-iteration loop the baseline recorded `charAt`, `length` and `isEmpty`
at **1 invocation each**, because the JIT had already taken the loop; only
`startsWith` (2,881) and `substring(I)` (173) were reached at all.

So the h2-bnf entries bought a slowdown and 20 divergences, and their own
record's premise ("a real fix that this gate gap left completely unreachable")
was measuring the wrong thing.

The four `CRATONVM_NATIVE_STRING_REGEX` shapes are the opposite case, which is
why they were kept: same interleaving, `probes/StringRegexCostProbe`, identical
digests, and no difference between the arms (`replaceAll` 58-77 ms baseline vs
58-68 ms here) against HotSpot's 118 ms. They are genuinely ~2x faster than
HotSpot and dropping them would have cost that.

## Verification

* **Both lists removed, strict boot clean** — L9's stated exit criterion. Done
  on a separate throwaway binary (`cratonvm-rkc16n6-nolists.exe`) before any
  other change, which is how the inertness above was established.
* **Both probes, both modes, HotSpot control, three runs each.**
  `JdkOnlyCensusLoadProbe` 9/9 and `JdkOnlyBreadthProbe` 15/15 with zero
  failures, 12 runs, exit status checked. The one residual against HotSpot is
  `DecimalFormat` grouping in the breadth probe, present in **both** CratonVM
  modes and therefore not a strict-mode defect.
* **The h2-bnf canary executes** — confirmed with a counter, not by reading
  the code, because that block was statically unreachable for months while
  reading exactly like working code. `--dump-native-registry` on
  `probes/StringForcedNativeCanaryProbe`: `startsWith` 2,881 invocations,
  `substring(I)` 173. Worth recording that `charAt`, `length` and `isEmpty`
  came back at **1 each** over a 120,000-iteration loop — the JIT bypasses them
  once the loop compiles, so the h2-bnf entries' real footprint is far smaller
  than their record implies.
* **The `--jdk-only` census reports zero `native-shadows-bytecode` violations
  for `java/lang/String`** — 0, from 12. The registry goes from **80
  `java/lang/String` rows to 26**: the 54 dropped are the `Bridge` surface,
  `intern()` survives with 5 invocations, and the five rows carrying
  `kind_stated: true` are exactly the `register_with_kind` calls this change
  introduced.
* **The matrix moved 57 -> 37 divergences: 20 fixed, 0 regressed**, and the
  `--real-jdk` and `--jdk-only` transcripts are byte-identical to each other.
  The 37 residuals are the three filed defects plus exception-message text; not
  one of them is a case the deleted natives had right.
* **`Compatible` byte-for-byte over `test_classes`** (contract section 5): all
  nine runnable entries, both binaries, exit status compared. **9 of 9
  identical** once a `best_ns=` timing figure and one interleaved WARN line are
  normalised; nothing functional differs.
* **Guards verified by injection**, per this feature's own failure log:
  `no_string_shape_is_forced_native_by_name_on_any_dispatch_path` and the
  source scan both fail when an arm is restored, and
  `the_string_guard_can_fail` proves the predicate is not simply `false` for
  everything by pinning the one entry (`StringUTF16.getChars`) that is still
  deliberately forced.

The source scan earned its keep on its first run: it flagged a
`java/lang/String` arm in `invoke_or_native` that turned out to be a
`setOption` **suppression** (`return Ok(None)`), the opposite of a force. The
scan is now keyed on the force-native disjunct shape rather than on any mention
of the class name, and the reason is written at the site.

## What this leaves open

* **`resolve_step1_native` still passes `bytecode_available: false`.** That is
  the actual section 1.4 hole this investigation walked into, and it is why the
  lists could be inert while the natives still won. Closing it would send
  **4,796** shadowing `Bridge` registrations to the bytecode under `--jdk-only`
  at once — far beyond this lane, and the reason the fix here is scoped to one
  class at the registration boundary. It belongs with L11/L12 section 11 and
  needs its own measurement.
* **`PatternSyntaxException.getMessage()` is `null`.** The real class declares
  only `(String desc, String regex, int index)`, so
  `create_exception_object`'s `<init>(String)` path cannot populate it. Getting
  the class right is what changes control flow; the description text is a
  separate, pre-existing gap that `Pattern.compile` already had.
* **The remaining `java/lang/String` `Intrinsic` rows** (`format`,
  `valueOf(Object)`, `codePointAt`, `regionMatches`, `chars`, `repeat`, ...)
  were adjudicated by somebody else, before `kind_stated` existed, and are
  untouched here. They are not part of the forced-native policy and want their
  own pass.
* **A full suite run** (Spring Boot / Tomcat / Hibernate) has not been done for
  this change. `java/lang/String` is on every path, so it is the change most
  likely to move something far away, and the suites run on a different host.
