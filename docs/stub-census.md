# Stub Census

Enforcement point: `vm/tests/tier1_tests.rs::t9_stub_audit_counts_match_census`
(the "T9 gate"). It counts stub-helper registrations across a fixed list of
`native-builtins/src/*.rs` files and asserts each category stays within a
ceiling.

**Direction**: counts should only go DOWN (a stub replaced with a real
implementation) or STAY THE SAME. A count going UP means a new stub was
added — the gate fails, which is the intended signal to review the addition
here before changing the ceiling.

## Categories

| Helper | Meaning | Spec-correct use |
|---|---|---|
| `native_noop` | Does nothing, returns `Ok(None)` | `registerNatives()V`, `initIDs()V`, no-arg `<init>()V` holders with no observable side effect, CDS archive dump/init natives (unsupported-optional feature) |
| `native_noop_with_this` | Does nothing, returns the receiver | Builder-style setter/no-op natives that must satisfy a fluent-return signature |
| `native_return_false` | Always returns `false` | `isSynthetic`/`isAnonymousClass`/`isLocalClass`/`isMemberClass`-style predicates that are never true for our loaded classes |
| `native_return_null` | Always returns `null` | (none left — see below) |
| `native_return_zero` | Always returns `0` | Int-returning natives with a spec-correct default/unsupported-hint state |
| `native_return_true` | Always returns `true` | Predicates unconditionally true for our loaded classes (`AccessibleObject.canAccess`) |

None of these are `todo!()`/`unimplemented!()`/panic stubs — those are covered
by `t13_no_unwrap_expect_panic_in_interpreter`.

## Current counts (2026-07-27, `fix/stub-removal-20260727`)

| Category | Before | After | Ceiling |
|---|---|---|---|
| `native_noop` | 56 | 47 | 47 |
| `native_noop_with_this` | 92 | 14 | 14 |
| `native_return_false` | 5 | 4 | 4 |
| `native_return_null` | 4 | **0** | 0 (must stay 0) |
| `native_return_zero` | 2 | 1 | 1 |
| `native_return_true` | (uncounted) | 1 | 1 |
| **Total** | **159** | **67** | **67** |

Repo-wide (not just the audited file list), registrations backed by a named
stub helper went from **233 to 82**.

Every category now has a ceiling. `native_noop_with_this`,
`native_return_null` and `native_return_zero` were previously uncapped —
that is how `with_this` reached 92 without anyone noticing.

## Corrections to the previous version of this file

The 2026-07-21 revision claimed a total of 219 against a ceiling of 250. The
gate's own arithmetic at that commit produced **159**. The doc had drifted
from the thing it documents; treat the gate's `[t9]` stderr line as the
source of truth, not this table.

Three defects in the gate itself were fixed alongside this sweep:

1. **`crypto.rs` was in the audited file list and does not exist.** The read
   used `unwrap_or_default()`, so a missing file silently contributed 0. The
   read is now a hard error naming the file.
2. **`native_return_true` was never counted**, despite being a
   constant-valued helper in `lib.rs` exactly like the other six.
3. **`tests_extracted.rs` is dead source.** There is no `mod tests_extracted;`
   anywhere in the tree, so it is never compiled and none of its
   registrations run. Its ~9 remaining hits are phantoms. It is deliberately
   left in the audited list so that wiring the file back in cannot smuggle in
   uncounted stubs.

## Known blind spots (NOT enforced by the gate)

The gate is line-based and only sees the named helpers in the listed files.
It does **not** see:

- **~525 inline constant closures** registered as natives across the repo —
  `r.register(cls, "m", "()V", |_ctx, _args| Ok(None));` and variants.
  Measured 2026-07-27: 240 `Ok(None)`, 129 `Ok(Some(Value::Object(None)))`,
  104 `Ok(Some(Value::Int(0)))`, 42 `Ok(Some(Value::Int(1)))`,
  10 `Ok(Some(Value::Long(0)))`. Heaviest: `lib.rs` 100,
  `tests_extracted.rs` 59 (dead), `jmx.rs` 34, `net_phase_e.rs` 30,
  `phases_late/concurrent.rs` 23, `phases_early.rs` 21.
- **Named helpers in other crates** — `native_noop_void` in
  `native-io/src/lib.rs`, and any helper defined outside
  `native-builtins/src`.
- **Named helpers in unlisted `native-builtins` files** — the list covers 13
  files; the crate has far more. `xnio_conduits.rs` alone held 18 stubs the
  gate never counted.
- **`native_method_handle_link_to`** (`lib.rs`), which backs all five
  `MethodHandle.linkTo*` registrations with a bare
  `Ok(Some(Value::Object(None)))`. A MethodHandle dispatch silently
  returning null is a real hazard; it is not a local fix and remains open.

So the honest statement is: the gate enforces a floor on one well-defined
subset, and the real constant-valued-native surface is roughly an order of
magnitude larger than the table above. Do not read "67" as "67 stubs left in
CratonVM".

## Deciding what to do with a stub

A registered native *shadows* the Java bytecode for that
class+method+descriptor, so a no-op silently disables a working method.
Two facts decide every case:

1. **Lookup keys on the DECLARING CLASS of the resolved method**, and
   interface *instance* methods are skipped
   (`vm/src/vm/vm_exec.rs`, `vm/src/runtime/interpreter.rs` step 6). So a
   native on an interface does not intercept user implementations — it only
   serves synthetic receivers. A native on an abstract/concrete CLASS *does*
   intercept a subclass that doesn't override. A native on an abstract
   method is dead. Caveat: an interface native IS reachable for a non-SAM
   method whose descriptor matches `(Liface;)Liface;` or `()Liface;`.
2. **There are two run modes.** Default is real-JDK. `--synthetic-jdk` (Cargo
   feature) loads no real class library — the synthetic natives *are* the
   class library. So DELETING a registration fixes real-JDK mode and BREAKS
   synthetic mode. Default to implementing; delete only when the
   registration is provably dead in both.

## When you add a stub

Confirm it is genuinely spec-correct constant behavior (not a placeholder for
real work that got deferred), write the justification comment at the
registration site, then raise the ceiling here and in `tier1_tests.rs` in the
same change. Prefer a thrown spec'd exception over a silent no-op whenever a
real implementation is out of reach — a caller learning the truth beats a
caller silently getting nothing.
