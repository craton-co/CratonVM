# `java.util.regex.Matcher`'s native bridge re-decodes the *entire* input string on every `find()`/`group()` call — real, but currently DEAD CODE in real-JDK mode

Status: **Bug confirmed + FIXED in the native bridge** (`native-builtins/src/lib.rs`,
uncommitted-then-committed 2026-07-11), but **that native bridge is not the active
dispatch path for a real-JDK `Pattern`/`Matcher` program**, so the fix currently has
no observable effect. The actual performance bug a user hits on today's default
build is a *different*, deeper issue — see
[`../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md`](../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md)
(now FIXED).

## Update (2026-07-11, later same day): successor fast path landed — this doc's bridge is STILL dead code, unrelated

After the substring fix above closed the O(n²) algorithmic bug, a separate,
*new* real-JDK-layout fast path
([`../internal/fixed-suite-bugs/matcher-find-realjdk-fastpath-FIXED.md`](../internal/fixed-suite-bugs/matcher-find-realjdk-fastpath-FIXED.md))
was added for `find()`/`find(int)`/`start()`/`end()`/`group()` to close the
remaining constant-factor gap. It is a completely separate set of natives
from the dead bridge this doc describes — it resolves every field BY NAME
against whatever real object layout is actually there (never a hardcoded
slot index), so it doesn't inherit this doc's real-vs-synthetic-layout
hazard. The dead bridge described below remains exactly as dead as before;
nothing in the new fast path un-drops or reactivates it.

## Correction (2026-07-11, same day)

The original version of this doc claimed the redecode bug below was the cause of a
user-reported quadratic slowdown in a `StringBuilder` + `Pattern.compile().matcher()`
+ `while (m.find()) { m.group(1); }` benchmark. That diagnosis was **wrong about which
code path is active**. Runtime instrumentation (an unconditional `eprintln!` placed at
the top of `native_matcher_find`/`native_matcher_group_idx`, rebuilt and run against
the exact reported repro) proved these functions are **never called** for a real-JDK
`Pattern`/`Matcher` program:

`native-api/src/registry.rs`'s `register()` silently **drops** every
`java/util/regex/Pattern`/`Matcher` native registration when
`drop_real_layout_synthetic` is set — and `vm/src/vm/vm_init.rs` sets that flag
unconditionally on the real-JDK arm, *before* `register_essential_natives` runs (so
the "essentials" registration calls this doc originally cited as evidence of forced
dispatch, `native-builtins/src/lib.rs:24944`/`50933`, are exactly the calls that get
dropped). The stated rationale (`registry.rs`) is the same "real-shaped object,
synthetic-index writes" family as
[`stringjoiner-synthetic-native-real-jdk-field-mismatch.md`](stringjoiner-synthetic-native-real-jdk-field-mismatch.md):
these legacy natives predate the real JDK's actual `Pattern`/`Matcher` field layout
and corrupt real-JDK-allocated objects if forced to run against them, so in real-JDK
mode `Pattern.compile()`/`Matcher.find()`/`group()` always run the **real loaded
bytecode** instead, by design.

The redecode bug described below is **real** — it's a genuine O(n²) defect in this
particular native bridge's source code — and the fix is real and committed. But
because the bridge is unconditionally dropped in real-JDK mode, neither the bug nor
the fix is currently reachable from any real-JDK program. It's kept (see
`native-builtins/src/lib.rs`'s `// --- Matcher natives ---` section header comment)
in case `drop_real_layout_synthetic` is ever narrowed or a non-default config
re-enables this bridge.

## Symptom (as originally reported — root cause below is corrected, symptom is real)

A tight `while (matcher.find()) { ...; matcher.group(N); }` loop over a
single, pre-built `String` gets progressively slower **per match** as the
string grows. Real HotSpot stays flat; CratonVM's per-call cost grows with the
*total* string length, producing worse-than-linear total time for O(n) matches
over an O(n)-length string. **This symptom is real and still open** — see the
substring/allocation doc linked above for the corrected root cause.

## The native-bridge bug (real defect, currently unreachable)

`java.util.regex.Pattern`/`Matcher`, when *not* dropped (i.e. in whatever build
configuration would leave `drop_real_layout_synthetic` unset for these classes),
are served by a **synthetic native bridge**. Its defect: `matcher_read_input()`
(`native-builtins/src/lib.rs`, in the `// --- Matcher natives ---` section)

```rust
fn matcher_read_input(ctx: &mut dyn NativeContext, mat: ObjectRef) -> String {
    match ctx.get_field(mat, MAT_FIELD_INPUT) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    }
}
```

fully decoded the Matcher's entire input `String` (the Java heap's UTF-16
`char[]`/Latin-1 `byte[]` → a fresh Rust `String`, an O(n) copy) **from scratch
on every single native dispatch**, not once per `Matcher` — called by
`native_matcher_find`, `native_matcher_group`/`group_idx`,
`matcher_group_boundary` (`start(N)`/`end(N)`), `native_matcher_matches`,
`native_matcher_looking_at`. `group(N)` for `N != 0` additionally re-ran the
regex search (`re.captures(&input[start..])`) from the last match's start to
the end of the string on every call — a second O(remaining-length) scan on
top of the redundant decode.

### Fix (committed, currently inert)

`matcher_read_input_cached()` replaces `matcher_read_input()`: caches the
decoded `Arc<str>` per-Matcher, keyed by `ctx.identity_hash_code(matcher)`
(a value CratonVM's GC explicitly carries across a move — the same
GC-move-stable property `register_var_handle_root` already relies on
elsewhere in this VM — rather than by raw `ObjectRef`, which a moving GC can
both relocate and, after freeing an address, reissue to an unrelated object).
Validated against `MAT_FIELD_INPUT`'s own identity hash so a
`reset(CharSequence)` swap is still caught. `find()` additionally now uses
`re.captures()` instead of `re.find()` and caches the resulting capture-group
spans (keyed the same way, plus the match's own start offset), so a following
`group(N)`/`start(N)`/`end(N)` on the same match is an O(1) lookup instead of
a second full regex re-search — with an always-correct re-search fallback for
any match-producing call (`find(int)`, `matches()`, `lookingAt()`, `region()`)
that doesn't populate the cache.

Two earlier cache designs were tried and found ineffective before landing on
identity-hash keying — worth recording since the failure mode is subtle and
easy to reintroduce: keying by raw `ObjectRef` and gating on an unchanged
`ctx.gc_collection_count()` is *correct* (never returns wrong data) but has
near-zero hit rate under any GC-active workload, since a benign relocation
(same logical object, new address — the overwhelmingly common case, not an
address *reuse*) invalidates the cache identically to a genuine reuse. The
`gc_collection_count()`-gated version made no measurable performance
difference over no caching at all in a benchmark that triggers frequent young
collections. `identity_hash_code` fixes this because it's stable across a
relocation by construction, only changing (via fresh allocation) on a
genuinely different object — the cache actually hits.

## Severity of the native-bridge bug itself

Low in practice today (unreachable in the default real-JDK build). Would be
high if the bridge is ever un-dropped without a broader real-layout fix — but
per the registry.rs rationale, un-dropping it *without* first fixing the
real-JDK-layout corruption issue would reintroduce a worse (correctness, not
just performance) bug, so this fix is not a prerequisite for anything
currently planned.
