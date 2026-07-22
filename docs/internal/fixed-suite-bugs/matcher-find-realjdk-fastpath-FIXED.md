# `java.util.regex.Matcher.find()`/`start()`/`end()`/`group()` real-JDK-layout fast path — FIXED

Status: **Landed** (2026-07-11), default-ON, opt-out `CRATONVM_NATIVE_MATCHER_FIND=0`.

## Symptom / context

After the O(n²) algorithmic bugs in `String.substring` and the (dead-in-real-mode)
legacy `Matcher` bridge were fixed (see
[`substring-large-parent-quadratic-allocation-FIXED.md`](substring-large-parent-quadratic-allocation-FIXED.md)
and
[`matcher-native-full-input-redecode-quadratic-FIXED.md`](matcher-native-full-input-redecode-quadratic-FIXED.md)),
the README's `String/Regex (10K)` QuickBench kernel still showed a **37.6x**
constant-factor gap vs HotSpot (11 ms JDK / 414 ms CratonVM). That gap is the
interpreted `java.util.regex` engine itself: `CRATONVM_NATIVE_STRING_REGEX`
(default-ON, see
[`../../internal/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md`](wildfly/bug-03-regex-perf-deployment-build.md))
already routes `String.replaceAll`/`replaceFirst`/`matches` to a fast cached
Rust `regex`/`fancy-regex` native, but does nothing for the extremely common
explicit
```java
Matcher m = pattern.matcher(text);
while (m.find()) {
    ...; m.group(1); ...
}
```
idiom — that still ran the real, interpreted `Matcher`/`Pattern` bytecode
state machine.

## Fix

`CRATONVM_NATIVE_MATCHER_FIND` (`native-builtins/src/lib.rs`, the
"java.util.regex.Matcher — real-JDK-layout `find()`/`find(int)` fast path"
section) adds Rust natives for `Matcher.find()`, `find(int)`, `start()`,
`start(int)`, `end()`, `end(int)`, `group()`, `group(int)` that operate
directly on the **real** OpenJDK `Matcher`/`Pattern` object layout — every
field is resolved BY NAME via `resolve_field_index` (cached once per process,
never a hardcoded slot number), so unlike the legacy dead-in-real-mode
bridge this cannot corrupt a real object regardless of its actual field
layout.

Design points:

- **UTF-16↔UTF-8 bridging.** Java `Matcher` indices are UTF-16 code-unit
  offsets; the `regex`/`fancy-regex` crates report UTF-8 byte offsets. A
  `byte_to_utf16`/`utf16_to_byte` offset-table pair is built once (O(n)) per
  distinct `text` object and cached, giving O(1) exact conversion in both
  directions — needed for correctness on any non-ASCII/astral input, not
  just performance.
- **`this.first`/`this.last` are the match's actual bounds, not the search
  resume position.** Real `Pattern$Start.match`'s own scan loop overwrites
  `matcher.first` as it tries each candidate position, so for an unanchored
  pattern the eventual `first` is commonly LATER than where the scan began
  (e.g. `(a+)(b)` found at index 2 while the scan started at index 0). An
  earlier draft conflated these two, which silently broke `start()`,
  `appendReplacement` (drops all literal text between matches), and
  `Matcher.replaceAll` (which loops via `find()` internally) — caught by the
  parity battery described below, not by any narrower unit test.
- **Bail-to-real-bytecode escape hatch.** Anything this fast path can't
  faithfully reproduce — a non-`String` `CharSequence` `text`,
  `transparentBounds(true)`, `anchoringBounds(false)`, a group-count mismatch
  between the compiled Rust regex and the real `Pattern`'s own
  `capturingGroupCount` — makes the native decline via
  `ctx.invoke_virtual_bytecode_only`, re-entering the same (class, method,
  descriptor) triple with the native-override check skipped, i.e. running
  the real bytecode body directly. Zero correctness risk for anything
  outside the accelerated subset, just no speedup for that call.
- **Single consolidated per-matcher cache.** One `Mutex`-guarded
  `HashMap<matcher_identity, {decoded text, offset tables, compiled regex}>`
  entry, validated against both the `text` and `parentPattern` object
  identities (so `reset(CharSequence)` / `usePattern(Pattern)` correctly
  invalidate it), replaces what was originally two separate caches (one for
  text, one via `compile_java_regex`'s own lock) — halves the per-`find()`
  lock count. `JavaRegex` clone is cheap (the underlying `regex`/
  `fancy-regex` engines are internally reference-counted).
- **`start`/`end`/`group` included, not just `find`.** They only read
  `first`/`last`/`groups[]`, which `find`/`find(int)` already populate
  correctly — no regex re-run, no text re-decode. `group(int)` delegates the
  actual character extraction to the receiver `text`'s own (already-fast)
  `String.substring(int,int)` via `invoke_virtual` rather than re-deriving a
  UTF-8 slice from this fast path's own tables.

### `NativeKind::Intrinsic`, not `Bridge` — a real regression caught during validation

The real-JDK-mode registry drop that suppresses the legacy synthetic-layout
`Matcher` natives (`native-api/src/registry.rs`, keyed on class name alone)
needed an exception for this new registration. The exception was first
written keying on `self.current_category == NativeKind::Bridge` — but the
legacy natives it's meant to suppress are *themselves* registered under
`Bridge` in the real-JDK build (inherited from a persistent
`set_category(Bridge)` far above their own registration site). Using
`Bridge` for the new exception too made it match BOTH registrations,
silently un-dropping the legacy slot-index bridge alongside the new one and
corrupting every real `Matcher` object it touched — symptom: `Matcher.start()`
throwing `IllegalStateException` after a second `find()`, reproduced even
with `CRATONVM_NATIVE_MATCHER_FIND` unset (i.e. with the new fast path not
even registered). Root-caused via a from-scratch minimal repro
(`(a).matcher("aaa")`, two `find()` calls, `start()`) that failed
identically with the flag on or off, which ruled out the new native's own
logic and pointed at something unconditional. Fixed by registering under
`NativeKind::Intrinsic` instead — confirmed nowhere else used for
`Pattern`/`Matcher` registrations under `drop_real_layout_synthetic` in the
real-JDK build. Category-matching this way is inherently a bit fragile (a
future `set_category` reshuffle upstream of either registration site could
reintroduce the same collision), so both the registry.rs exception and the
`native-builtins` registration site carry an explicit warning comment
against changing the category without re-auditing.

## Verification

`MatcherParity.java` (141-line transcript battery covering zero-width
matches, regions, `transparentBounds`/`anchoringBounds`, named groups,
backreferences, lookahead/lookbehind, non-ASCII and astral (surrogate-pair)
input, `reset()`/`reset(CharSequence)`/`find(int)`/`usePattern`/`region`
state interplay, `appendReplacement`/`appendTail`, `Matcher.replaceAll`,
illegal-state and bad-group-index exception checks, `toMatchResult`, and the
`results()` stream) is byte-identical to HotSpot both with
`CRATONVM_NATIVE_MATCHER_FIND` at its new default and with the `=0` opt-out,
except:
- The documented `hitEnd()`/`requireEnd()` approximation (see the module
  banner in `native-builtins/src/lib.rs`): real HotSpot's value depends on
  which internal `Pattern$Node` subclass the compiler chose for a given
  pattern (e.g. a greedy quantifier's expansion reaching the region boundary
  vs. a literal/Boyer-Moore node concluding definitively) — verified against
  the battery that a plain "match end == region end" boundary check is
  neither uniformly right nor uniformly wrong (JDK itself returns both
  `true` and `false` for different patterns whose match happens to end
  exactly at the region boundary), so no cheap boundary-only heuristic can
  close this without reimplementing HotSpot's backtracking engine. Narrow
  impact in practice: consulted almost exclusively by `java.util.Scanner`'s
  stream-refill decision, which this fast path's fully-buffered-`String`
  requirement makes moot (there is no "more data" to pull either way).
- The pre-existing Windows-console non-ASCII display artifact (CratonVM's
  actual string content is correct; only the terminal's rendering of it
  differs — same residual `CRATONVM_NATIVE_STRING_REGEX` already documented).

## Performance

`bench/StringRegexOnly.java` (`StringBuilder`-built `"key0=value0;key1=..."`
input + `Pattern.compile("(\\w+)=(\\w+);").matcher(text)` +
`while (m.find()) { m.group(1); m.group(2); m.start(); m.end(); }`),
best-of-5, n=10,000 entries:

| | JDK 25 | CratonVM (flag off / real bytecode) | CratonVM (flag on / this fix, new default) |
|--|--|--|--|
| Time | 8 ms | 3,014 ms | **153 ms** |
| Ratio vs JDK | 1x | 377x | **19.1x** |

Progression during development (same benchmark, each step's dominant fix):

| Step | Best (ms) | Ratio vs JDK |
|--|--|--|
| Naive by-name field access (`get_field_by_name`/`set_field_by_name` for every field, every call) | 950 | ~95x |
| + cache resolved field indices, use index-based `get_field`/`set_field` | 683 | ~68x |
| + consolidate text-cache and regex-compile-cache into one single-lock cache | 507 | ~51x |
| + accelerate `start`/`end`/`group` (not just `find`) | 153 | **19.1x** |

`get_field_by_name`/`set_field_by_name` each take a `class_manager`
`RwLock::read()` and walk the class hierarchy searching for the field by
name on **every call**, not just the first — a single `find()` touches
~13 `Matcher` fields, so this was the dominant cost of a naive
implementation, worse than the regex search itself. `get_field(obj, index)`
resolves through a per-class-id cached descriptor lookup with no lock (see
`vm/src/vm/vm_exec.rs`'s `get_field`/`get_field_by_name`); since
`java.util.regex.Matcher`/`Pattern` are bootstrap classes (one loaded
definition VM-wide, layout fixed for the process lifetime), resolving each
field's index once and reusing it is safe.

### Residual gap (153ms vs 8ms — not fully closed)

19.1x is a large improvement over 37.6x but not down to the 5-7x range the
other constant-factor QuickBench rows (Arithmetic/Fibonacci/Sieve/Matrix) sit
in. The remaining cost is believed to be the same **fixed per-native-call VM
dispatch overhead** the HashMap section of the main README documents
(conservative JIT-frame root scanning, RwLock-guarded class/field-layout
lookups, generic dispatch-machinery overhead — measured there as NOT
allocation/GC-bound, ~29% of sampled stacks in root-scanning alone) — each
`find`/`group`/`start`/`end` call in the benchmark's loop body is a separate
native dispatch, so the loop pays that fixed tax five times per iteration
regardless of how cheap each native's own body is. Closing this further
would need the broader native-call-dispatch-overhead fix HashMap is also
waiting on (not yet attempted anywhere in this repo), not another
regex-specific change — the regex-specific work (algorithmic fix, ASCII
Perl-class parity, this fast path) is believed complete.
