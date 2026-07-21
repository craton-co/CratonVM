# Stub Census

Enforcement point: `vm/tests/tier1_tests.rs::t9_stub_audit_counts_match_census`
(the "T9 gate"). It counts stub-helper registrations across
`native-builtins/src/{lib,phases_late,phases_early,crypto,tls,serialization,
jmx,http2,servlet,cds,aot,classfile_api,lang_string,tests_extracted}.rs` and
asserts the counts stay within a ceiling per category.

**Direction**: counts should only go DOWN (a stub replaced with a real
implementation) or STAY THE SAME. A count going UP means a new stub was
added — the gate fails, which is the intended signal to review the addition
here before bumping the ceiling in `tier1_tests.rs`.

> This file was deleted in commit `c74a51872` (2026-04-26) along with other
> stale session-progress docs, even though the T9 gate's own comment still
> points at it. This is a rewritten, current version — it does not attempt
> to reproduce the old file's per-natives-by-name breakdown (see
> `git show c74a51872 -- docs/stub-census.md` if that historical detail is
> ever needed); it tracks the counts the gate actually enforces.

## Categories

| Helper | Meaning | Spec-correct use |
|---|---|---|
| `native_noop` | Does nothing, returns `Ok(None)` | `registerNatives()V`, no-arg `<init>()V` holders with no observable side effect, CDS archive dump/init natives (unsupported-optional feature) |
| `native_noop_with_this` | Does nothing, returns the receiver (`this`) | Builder-style setter/no-op natives that must satisfy a fluent-return signature |
| `native_return_false` | Always returns `false` | `isSynthetic`/`isAnonymousClass`/`isLocalClass`/`isMemberClass`-style predicates on classes/methods that are never true for our loaded classes |
| `native_return_null` | Always returns `null` | `getPackage`/`getProvider`/`getAnnotation`-style accessors with no backing data |
| `native_return_zero` | Always returns `0` | Int-returning natives with a spec-correct default/unsupported-hint state |

None of these represent `todo!()`/`unimplemented!()`/panic-stub code — those
are covered by a separate gate (`t13_no_unwrap_expect_panic_in_interpreter`
in `vm/src/runtime/interpreter.rs`, currently enforcing zero such markers in
production code paths). These are all real, spec-correct implementations
that happen to be constant-valued.

## Current counts (2026-07-21, `dev` @ post-`3e1b4ac62`)

| Category | Count | Ceiling |
|---|---|---|
| `native_noop` | 65 | 65 |
| `native_noop_with_this` | 136 | (uncapped) |
| `native_return_false` | 12 | 15 |
| `native_return_null` | 4 | (uncapped) |
| `native_return_zero` | 2 | (uncapped) |
| **Total** | **219** | **250** |

The `native_noop` count grew from a documented baseline of 54 (ceiling 55,
set at the initial open-source commit `a6dc911ed`, 2026-04-26) to 68 raw /
65 corrected as of this audit. The growth is legitimate — no case of a real
implementation being silently replaced by a no-op was found. Contributing
commits: `f5393c4825`, `9308959c9d`, `be787d8620`, `687cefebb4`,
`1416534ca4`, `a1bf33fb72`, `a37288604a`, `b448f20391`, `057f826641`,
`5711264d7f` — mostly CDS archive-dump/-init natives, `ProcessImpl`/process-
controller no-ops, and BC crypto no-arg `<init>()V` holders (the real init
lives in a separately-registered native).

Two counting bugs in the T9 gate itself (fixed alongside this doc, same
session) previously inflated the raw `native_noop` count by 3: a private
`fn native_noop_return_this(...)` definition line (not excluded — only
`pub fn`/`pub(crate) fn` were) and a rustfmt-wrapped multi-line
`use crate::{ ..., native_noop, ... };` import continuation line (not
excluded — only lines starting with `use ` were). The gate now tracks
`use { ... }` block state across lines and excludes bare `fn` definitions.

When you add a new stub that pushes a count past its ceiling: confirm it's
genuinely spec-correct constant behavior (not a placeholder for real work
that got deferred), then bump the ceiling in `tier1_tests.rs` and update the
table above in the same change.
