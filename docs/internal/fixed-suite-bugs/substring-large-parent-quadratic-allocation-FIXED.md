# `String.substring()` on a large parent scaled O(n²), not O(n) — FIXED

Status: **FIXED** (2026-07-11). Root cause: the *active* real-JDK-mode native for
`String.substring(int,int)`/`(int)` was a buggy inline closure that fully decoded
the parent string on every call, unrelated to any dispatch-gate/GC mechanism this
investigation initially chased. See "Root cause — finally found" below for the
real story; the sections above it are kept as an accurate record of a long, mostly
wrong investigation, since the false leads (and how they were eventually ruled
out) are exactly the kind of thing a future session re-hitting this class of bug
should see instead of re-deriving from scratch.

Found: 2026-07-11, as the corrected root cause behind a user-reported
`StringBuilder` + `Pattern.compile().matcher()` + `while (m.find()) { m.group(1); }`
benchmark whose CratonVM-vs-JDK-25 slowdown ratio grew with input size (18.8× at
1,000 entries → 94.4× at 5,000 → 238.8× at 10,000; did not finish in 60s at
50,000). The original investigation misattributed this to a bug in `Matcher`'s
native bridge (see
[`matcher-native-full-input-redecode-quadratic.md`](matcher-native-full-input-redecode-quadratic.md),
corrected) — that bridge is provably **not** the active dispatch path in
real-JDK mode. This doc covers the actual, now-fixed bug.

## Symptom

Extracting many small (fixed-size) substrings from one large, already-built parent
`String` got progressively slower **per extraction** as the parent grew — even
though each individual extraction only touches a small, bounded number of
characters. Reproduced with **zero regex/Matcher code** — pure `String.substring()`.

## Quantification

P-core-pinned (Windows) / idle (Linux Azure host), checksums identical to JDK-25
at every size (was always a pure perf bug, never a correctness bug).

`bench/SubstringOnly.java`: build a parent `String` of length ~10n *outside* the
timed region, then loop n times extracting a 5-char substring at an advancing
offset — no regex, no Matcher, nothing but `StringBuilder` (confirmed correctly
linear) and `String.substring`:

| entries | before | after fix | JDK-25 |
|---|---|---|---|
| 1,000  | 14 ms   | 2 ms   | 1 ms |
| 5,000  | 316 ms  | 13 ms  | 1 ms |
| 10,000 | 1283 ms | 29 ms  | 2 ms |
| 50,000 | (not tested — extrapolates to tens of seconds) | 157 ms | 5 ms |

Per-entry cost before the fix roughly doubled every time n doubled (the O(n²)
signature: 0.014 → 0.063 → 0.128 ms/entry). After the fix it's flat (0.002 →
0.0026 → 0.0029 → 0.0031 ms/entry) — genuinely linear, matching JDK's own shape.

The original combined benchmark (`StringRegexOnly`/`StringBuilder` +
`Pattern`/`Matcher` `find()`+`group()`) improved from 226/1699/4775 ms (never
finished at 50,000) to 41/213/416/2295 ms at 1K/5K/10K/50K — the *algorithmic*
complexity bug (unbounded ratio growth) is gone; what's left is the
already-documented, much smaller constant-factor gap for interpreted
`java.util.regex` (see
[`../internal/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md`](../internal/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md)),
a separate, lower-severity, already-tracked issue.

## Root cause — finally found

This investigation went through two wrong turns before finding the real bug —
both are worth understanding since the pattern (multiple natives with the same
method signature, only one of which is actually reachable) is easy to re-hit:

1. **First wrong turn**: assumed the bug was in `Matcher`'s native bridge
   (`matcher_read_input`). Runtime instrumentation proved that bridge is
   unconditionally dropped in real-JDK mode
   (`drop_real_layout_synthetic` in `native-api/src/registry.rs`) — dead code.
2. **Second wrong turn**: found and instrumented `native_string_substring`
   (`native-builtins/src/lang_string.rs`) — a *correctly implemented*,
   properly-bounded substring native ("read only the requested range, don't
   materialize the whole String first") — and confirmed via `eprintln!`
   instrumentation it was **never called**. Chased this as a dispatch-gate
   mismatch (`force_native_over_real_jdk_bytecode` vs. `check_override` in
   `vm/src/vm/vm_exec.rs`), attempted a gate fix, rebuilt, retested: **zero
   effect**. Traced the actual CacheMiss fallback chain with gdb on a fresh
   Linux host (`execute_invokevirtual_vtable_fast` → on CacheMiss →
   `execute_invoke` → `execute_invoke_kind` → `try_stackless_invoke`, NOT
   `invoke_on_class_shared_inner`/`check_override` as assumed) — this was real,
   useful groundwork, but the reason `native_string_substring` specifically was
   unreachable turned out to be much simpler and unrelated to any of it: **it's
   registered inside `register_synthetic_overrides`, which is
   `#[cfg(feature = "synthetic-jdk")]`-gated and not compiled into the default
   real-JDK build at all.** No amount of runtime dispatch tracing on a real-JDK
   binary was ever going to reach a function that isn't even linked in.

3. **The actual bug**: `register_essential_natives` (`native-builtins/src/lib.rs`,
   the real-JDK-mode registration path) has its **own**, separate, inline-closure
   registrations for `java/lang/String.substring(II)Ljava/lang/String;` and
   `(I)Ljava/lang/String;` — these ARE the ones real-JDK mode actually dispatches
   to. Both closures did:
   ```rust
   let s = ctx.read_string(this).unwrap_or_default();   // decode the ENTIRE parent
   let chars: Vec<char> = s.chars().collect();           // ANOTHER full-string pass
   let slice: String = chars[begin..end].iter().collect();  // then finally slice
   ```
   — an O(parent length) decode-and-collect on **every single call**, regardless
   of how small `[begin, end)` was. `n` substring calls over an `n`-length parent
   = `O(n)` per call × `O(n)` calls = `O(n²)`. Exactly the redecode-per-call
   pattern from the original (wrong) Matcher hypothesis — just in the actually-live
   native, not the dead one.

## Fix

`native-builtins/src/lib.rs`: replaced both inline closures with direct
delegation to the pre-existing, correctly-bounded implementations in
`lang_string.rs` (`native_string_substring` / `native_string_substring_one`),
which read only the requested `[begin, end)` range directly off the backing
`char[]`/`byte[]` array (peeking at its length for bounds-checking) instead of
decoding the whole parent first. These functions already existed — with a doc
comment describing this exact optimization — but were only reachable from the
`synthetic-jdk`-gated registration path, never from the real-JDK one that
actually needed them.

Verified: checksums identical to JDK-25 at every tested size (1K/5K/10K/50K,
both `SubstringOnly` and the original combined regex benchmark); `cargo test
--release -p cratonvm-native-builtins --lib` — same 5 pre-existing failures
with and without the fix (confirmed via `git stash`/re-run, all unrelated:
jspecify type-use annotations, a ByteBuffer bulk-copy test, and a
`${VAR}`-substitution security-manager-policy test), zero new regressions.

## Residual, secondary finding (not fixed this session, low priority)

The `force_native_over_real_jdk_bytecode` fast-path allowlist
(`vm/src/runtime/interpreter.rs`) genuinely doesn't include
`java/lang/String.substring`, while `check_override`
(`vm/src/vm/vm_exec.rs`'s slow path) does — a real inconsistency between the
"two gates [that] should agree" per the codebase's own documentation. This
inconsistency turned out to be irrelevant to the O(n²) bug (dispatch for
`substring` never goes through either of those gates in practice — it resolves
via `try_stackless_invoke`, a third, separate mechanism this investigation
found along the way), so the gate-alignment fix landed alongside the real one
is a harmless, independently-defensible cleanup, not part of the load-bearing
fix. Worth revisiting if a future dispatch-consistency audit is done, but
nothing currently depends on it.

## Severity

Was high — `String.substring()`/`Matcher.group()`-from-a-large-parent is an
extremely common pattern (any parser, tokenizer, log scraper, or CSV/JSON
reader that builds one large buffer and repeatedly extracts small fields from
it). Now fixed for the default real-JDK build path.
