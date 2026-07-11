# `java.util.regex.Matcher`'s native bridge re-decodes the *entire* input string on every `find()`/`group()` call — O(n²) instead of O(n)

Status: OPEN — confirmed, root-caused, quantified; not fixed (fix direction requires a
GC-safe cross-call cache, not attempted this session)

Found: 2026-07-11, while investigating a user-reported benchmark
(`bench/StringRegexOnly.java`: build an N-entry `StringBuilder`, then loop
`Pattern.compile("value(\\d+),").matcher(text)` + `while (m.find()) { ...
m.group(1) ... }`) whose CratonVM-vs-JDK-25 slowdown ratio grew with input
size instead of staying constant (18.8× at 1,000 entries → 94.4× at 5,000 →
238.8× at 10,000; did not finish in 60s at 50,000) — the classic tell for an
algorithmic complexity bug, not a constant-factor interpreter/JIT slowdown.

## Symptom

A tight `while (matcher.find()) { ...; matcher.group(N); }` loop over a
single, pre-built `String` gets progressively slower **per match** as the
string grows, even though every individual match only requires local work
(scan forward from the last match end, extract a bounded-size substring).
Real HotSpot stays flat; CratonVM's per-call cost grows linearly with the
*total* string length, producing O(n²) total time for O(n) matches over an
O(n)-length string.

## Quantification

Isolated `RegexOnly` (build the `String` **outside** the timed region, time
only the `Pattern.compile` + `while (m.find()) { m.group(1); }` loop) vs.
`StringBuilderOnly` (time only the append loop, no regex) confirms the
quadratic behavior is 100% in the Matcher/Pattern path, not `StringBuilder`
(whose `sb_ensure_capacity` amortized-doubling growth is correct and scales
linearly — see `native-builtins/src/lang_string.rs:1472`).

P-core-pinned (`ProcessorAffinity=0xFFFF`), `target/release/cratonvm.exe`,
JDK-25, checksums identical at every size (pure perf bug, not correctness):

| entries | `StringBuilderOnly` (CratonVM) | `RegexOnly` CratonVM | `RegexOnly` JDK-25 | ratio |
|---|---|---|---|---|
| 1,000  | 2 ms  (linear: 0.002 ms/entry) | 79 ms   | 6 ms  | 13.2× |
| 5,000  | 10 ms (linear: 0.002 ms/entry) | 662 ms  | 8 ms  | 82.8× |
| 10,000 | 20 ms (linear: 0.002 ms/entry) | 1980 ms | 11 ms | 180×  |

`StringBuilderOnly` is perfectly linear (0.002 ms/entry at every size).
`RegexOnly`'s CratonVM per-entry cost grows with n (0.079 → 0.132 → 0.198
ms/entry) while JDK's stays flat — the signature of an O(n²) algorithm on
CratonVM's side competing against JDK's O(n). These numbers reproduce the
original combined-benchmark ratios (18.8×/94.4×/238.8×) almost exactly once
`StringBuilder`'s (correctly linear, negligible) cost is subtracted out.

## Root cause

`java.util.regex.Pattern`/`Matcher` are served by a **synthetic native
bridge**, not real JDK bytecode — `register_regex_natives`
(`native-builtins/src/lib.rs:50933`) is called unconditionally from the
essentials registration path (`native-builtins/src/lib.rs:24944`, comment:
"Keep regex natives in essentials too... Pattern.compile(String,int) is
always reachable") and forced over real bytecode during bootstrap per the
comment at `native-builtins/src/lib.rs:50934-50940`. This is the same
"real-shaped object, synthetic dispatch" family as
[`stringjoiner-synthetic-native-real-jdk-field-mismatch.md`](stringjoiner-synthetic-native-real-jdk-field-mismatch.md)
(allocated with the real `java/util/regex/Matcher`/`Pattern` class id, but
driven by hardcoded legacy field indices `MAT_FIELD_*`/`PAT_FIELD_*` at
`native-builtins/src/lib.rs:49590-49604`) — a different bug in the same
"synthetic bridge instead of real bytecode" architecture, not the same
symptom (this one is a pure perf bug; the StringJoiner one is silent wrong
content).

The actual defect: `matcher_read_input()` (`native-builtins/src/lib.rs:51308`)

```rust
fn matcher_read_input(ctx: &mut dyn NativeContext, mat: ObjectRef) -> String {
    match ctx.get_field(mat, MAT_FIELD_INPUT) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    }
}
```

fully decodes the Matcher's entire input `String` (the Java heap's
UTF-16 `char[]`/Latin-1 `byte[]` → a fresh Rust `String`, an O(n) copy —
`read_string` in `vm/src/vm/vm_exec.rs:3667` walks the whole backing array)
— and it is called **from scratch on every single native dispatch**, not
once per `Matcher`:

- `native_matcher_find` (`lib.rs:51325`) calls it at line 51330, on every
  `find()` invocation.
- `native_matcher_group`/`native_matcher_group_idx` (`lib.rs:51474`,
  `51497`) call it again at lines 51492/51512, on every `group()`/`group(N)`
  invocation — and `group(N)` for `N != 0` **also re-runs the regex search**
  (`re.captures(&input[start..])`, line 51529) from the last match's start
  to the end of the string, a second O(remaining-length) scan on top of the
  redundant decode.
- The same pattern repeats in `matcher_group_boundary` (`start(N)`/`end(N)`,
  `lib.rs:51537`), `native_matcher_matches`, `native_matcher_looking_at`.

The user's loop calls both `find()` and `group(1)` once per match, so it
hits this **twice** per iteration. With ~n matches over an ~n-length input,
total work is `sum_{i=1}^{n} O(n)` (full redecode each call) `= O(n²)`,
independent of the regex engine itself (`compile_java_regex`, `lib.rs:50147`,
**is** properly cached by `(pattern, flags)` and is not the bottleneck —
confirmed by `RegexOnly`'s growing *per-entry* cost, which a flat compile-cache
lookup cost cannot explain).

Real JDK's `java.util.regex.Matcher.find()` never re-copies the whole
`CharSequence`; it walks the existing backing array from the stored `from`
position via `charAt`/direct indexing, which is why JDK's own numbers stay
flat (6/8/11 ms) while CratonVM's climb (79/662/1980 ms) on the *identical*
Java source.

## Why a naive fix is unsafe

The obvious fix — decode `MAT_FIELD_INPUT` once (e.g. in
`native_pattern_matcher`/`reset()`) and cache the Rust `String` for reuse by
later `find()`/`group()` calls on the same `Matcher` — cannot key the cache
by the Matcher's or input String's `ObjectRef` directly:  `ObjectRef` is a
raw heap pointer (`types/src/value.rs:62`, `NonNull<u8>`), not a stable
handle, and CratonVM's GC moves objects (see the `pin_native_root`/
`read_native_pin` dance `sb_ensure_capacity` uses for exactly this reason,
`native-builtins/src/lang_string.rs:1493-1499`). Several of these Matcher
natives call `ctx.create_string` (which can itself trigger a GC) while a
cache lookup/store would be live, and after any GC a since-freed arena
address can be **reissued to a different object** — a pointer-keyed cache
would then silently return a different object's cached content: a
correctness bug, not just a stale-cache miss. A safe fix needs either a
GC-epoch-qualified cache key, or to store the decoded form as a proper
GC-managed side-object reachable from the Matcher (so the GC keeps it
consistent), or to change the whole regex bridge to operate on a live
zero-copy view of the array (`heap.array_data_ptr`, already used by
`bulk_array_copy`) instead of an owned decoded `String` at all.

## Suggested fix direction (not attempted this session)

Most robust: change `matcher_read_input`'s callers to slice the *existing*
backing array via a GC-safe zero-copy accessor (mirroring
`vm/src/vm/vm_exec.rs`'s `array_data_ptr` fast path used by
`bulk_array_copy`) re-fetched fresh on every call (cheap — no decode, just a
pointer + length), rather than owning a decoded `String` at all. This also
sidesteps the cache-safety problem above entirely, at the cost of needing
the `regex` crate (or a hand-rolled matcher) to search over UTF-16 code
units directly instead of a UTF-8 `Rust String`. A cheaper, narrower interim
mitigation: at minimum, have `group(N)`'s capture re-search only scan
`&input[start..end_hint]` bounded by a reasonable upper bound instead of
`&input[start..]` to the end of the string, and avoid the *second*
full-string redecode+recompile-cache-lookup on `group()` by having `find()`
stash the already-decoded input/compiled-regex for reuse within the same
Matcher generation — but any per-Matcher stash still needs the GC-safety
treatment above.

## Severity

High for any regex-heavy workload with many matches over a long string
(log parsing, tokenizers, template engines, `AssetUtil.getFullPathForClassResource`-style
per-class regex scans during archive building — see
[`../internal/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md`](../internal/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md),
which already documents severe regex slowness via a different, older
mechanism (real-bytecode interpreter/native-charAt-bridging costs) — that
doc's analysis pre-dates/doesn't cover this synthetic-bridge native path,
which is a separate, likely now-dominant cost on the current default
config. `Pattern.compile()`/`Matcher.find()` loops are extremely common
(any hand-written parser, `Scanner`-style tokenizer, log scraper), so this
is a general and easily hit performance cliff, not an edge case.
