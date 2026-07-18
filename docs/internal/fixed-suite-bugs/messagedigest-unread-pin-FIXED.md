# `MessageDigest` natives: `this`/accumulator used stale after a nested allocation — FIXED

Status: RESOLVED — fixed 2026-07-17
Severity: Low-to-moderate (silent, GC-timing-dependent data loss — a dropped
`update()` call or un-reset accumulator; no crash observed or expected from
this specific bug)
Fixed: 2026-07-17, branch `fix/messagedigest-unread-pin-20260717`, merged to
`dev` as `0eaf3f80` (fix commit `2071b77e`)

## Context

Found while auditing `native-builtins/src/` for the exact anti-pattern
confirmed real earlier the same day in `populate_real_thread_holder`
(`0e1a1fa2`,
[countdownlatch-thread-fieldholder-unread-pin-FIXED.md](countdownlatch-thread-fieldholder-unread-pin-FIXED.md)):
pin an object, make a nested call that can allocate (and therefore trigger a
moving GC), and then use the *original, un-re-read* pin afterward. The audit
was part of a session investigating the long-standing
`DefaultCatalogAndSchemaTest` `BigInteger` `ArrayIndexOutOfBoundsException`
(see
`docs/known-issues/hibernate/hib-misc-residuals-20260716.md`), on the
hypothesis that `NamingHelper.hashedName`'s `MessageDigest`/`BigInteger` call
chain might route through a native builtin with this defect. That specific
connection was **not** established (see the residuals doc for the full
mechanistic argument: `BigInteger`'s own native overrides are dead code for
`invokevirtual` dispatch, so the actual crash path never touches a native
builtin) — but the `MessageDigest` bug described here is real, independently
confirmed, and fixed regardless.

## The bug

`native-builtins/src/lib.rs`'s `java/security/MessageDigest` support has
three functions that each capture a heap `ObjectRef` as a bare Rust local,
then call an allocating `NativeContext` operation (`ctx.create_string` or
`ctx.new_array`, both of which can trigger a moving GC), and then use the
original local again — without re-reading it through a pin:

- **`native_md_get_instance`** (backs `MessageDigest.getInstance(String)`):
  allocates `md`, then calls `ctx.create_string(&algo)` and
  `ctx.set_field(md, MD_FIELD_ALGO, ...)`, then `ctx.new_array(...)` and
  `ctx.set_field(md, MD_FIELD_DATA, ...)` — `md` is reused across two
  further allocations with no re-read.
- **`md_append_bytes`** (backs `MessageDigest.update(byte[])`/`update(byte)`):
  reads the existing accumulator `old_data` from `this`, then calls
  `ctx.new_array(..., new_len)` to allocate the grown array, then copies
  `old_data`'s bytes into it and writes the result back to
  `this.data` — both `this` and `old_data` are used after the allocation
  with no re-read.
- **`native_md_digest`** (backs `MessageDigest.digest()`): after computing
  the digest, allocates the result array, then allocates a second, empty
  array to reset `this.data` — `this` is used for that final reset write
  after both allocations with no re-read.

`vm/src/vm/vm_exec.rs`'s `NativeContext` implementation confirms these
`ObjectRef`s are not automatically healed: `get_field`/`set_field`/
`get_array_element`/`set_array_element` dereference the raw `ObjectRef`
directly with no `load_and_forward` call. The only automatic healing in this
codebase's native-call machinery is (a) native-call **entry** arguments,
forwarded once in `safe_native_call_impl` before pinning, and (b) the
callback's final **return value**, healed by the same function after the
callback returns (the `cce0079` barrier). Neither covers an `ObjectRef`
captured and reused as a bare local **during** a callback's own execution,
across the callback's own nested allocating calls — exactly the gap
`populate_real_thread_holder` had.

**Effect**: if a GC lands in one of these unguarded windows and relocates
the object, the subsequent write lands on the object's abandoned from-space
copy — a valid write to already-reclaimed memory, no fault, but the live
copy never observes it. Concretely:
- `native_md_get_instance`: the `algorithm`/`data` fields on the live
  `MessageDigest` instance could remain unset (default/null), so a later
  `digest()` call would default the algorithm (`compute_digest`'s `_ =>
  real_sha256(data)` fallback) and/or hit the early-return null-data path.
- `md_append_bytes`: the grown accumulator write is lost, so `update()`
  silently becomes a no-op — a later `digest()` computes the hash of less
  data than the caller intended (in the worst case, of nothing at all, if
  the only `update()` call hits this window).
- `native_md_digest`: the "reset" write is lost, so a **reused**
  `MessageDigest` instance (multiple `update()`/`digest()` cycles on the
  same object) would silently include stale bytes from a previous digest.

None of these produce a crash or an obviously malformed object — they
produce a **silently wrong hash value**, which is why this was very unlikely
to explain the `BigInteger` `AIOOBE` this bug was originally being hunted
for (that bug's signature is a structurally valid-but-wrong-shape `int[]`
read from within `BigInteger`'s own real-bytecode arithmetic, not a
wrong-content digest fed into an otherwise-correct construction).

## Fix

Applied the same discipline as `populate_real_thread_holder`: pin the
object immediately (`ctx.pin_native_root`), re-read through the pin
(`ctx.read_native_pin`) immediately before every write that follows an
allocating call, and `ctx.unpin_native_roots` at the end of each function.

- `native_md_get_instance`: pin `md` right after allocation; re-read before
  the `ALGO` field write (after `create_string`) and again before the
  `DATA` field write (after `new_array`).
- `md_append_bytes`: pin `this` and the just-read `old_data` before the
  `new_array` call; re-read both immediately after it, before the copy loop
  and the final `set_field`.
- `native_md_digest`: pin `this` and the freshly-allocated `result` before
  the second `new_array` call (the reset-array allocation); re-read both
  immediately after it, before the final `set_field`/return.

## Verification

- `cargo test --release -p cratonvm-native-builtins --lib`: 3001 passed, 1
  failed. The one failure
  (`lang_system::checkexec_security_tests::denying_sm_blocks_runtime_exec_before_spawn`)
  is an unrelated, pre-existing environmental failure (`Runtime.exec failed:
  No such file or directory` — a missing test-fixture binary on this host),
  confirmed unrelated to this change (different subsystem: `Runtime.exec`/
  `SecurityManager`, not `MessageDigest`/GC).
- `cargo test --release -p cratonvm-native-builtins --lib -- messagedigest md_
  digest`: 8/8 passed (digest correctness against real HotSpot reference
  vectors — MD5/SHA-1/SHA-224/SHA-256/SHA-384/SHA-512 truncated variants —
  unaffected by the GC-safety fix, as expected: these tests don't exercise
  the GC-timing window).
- Full-harness `CratonRunner`/`selectClass` against
  `DefaultCatalogAndSchemaTest` (which exercises this exact code via
  `NamingHelper.hashedName`'s `MessageDigest.getInstance("MD5")` →
  `update()` → `digest()` chain, once per generated constraint name): 3
  clean runs pre-merge (`found=132 ok=132 failed=0`), 1 clean run post-merge
  onto `dev` (`found=132 ok=132 failed=0`). No regression in the DB-naming
  behavior this code path drives.
- No isolated, GC-stress-amplified repro was built specifically for this
  fix (e.g. a tight `update()`/`digest()` loop under
  `CRATONVM_DBG_GC_STRESS`) — the bug's effect (silently wrong digest bytes)
  has no simple assertable "did the bug fire" signal from Java code alone
  without a reference HotSpot comparison per call, and this was judged out
  of scope for the session that found it. A future session wanting to
  directly demonstrate the pre-fix defect could compare
  `MessageDigest.getInstance("MD5").also{it.update(bytes)}.digest()` against
  a precomputed reference hash across many trials under
  `CRATONVM_DBG_GC_STRESS`.

## Files

`native-builtins/src/lib.rs` (`native_md_get_instance`, `md_append_bytes`,
`native_md_digest`).

## Related

- `docs/known-issues/hibernate/hib-misc-residuals-20260716.md` — the
  `DefaultCatalogAndSchemaTest` `BigInteger` `AIOOBE` investigation this fix
  was found as a side effect of. That item remains separately OPEN; this fix
  does **not** close it (see that doc's corresponding session update for the
  full mechanistic argument for why not).
- [countdownlatch-thread-fieldholder-unread-pin-FIXED.md](countdownlatch-thread-fieldholder-unread-pin-FIXED.md)
  — the same-day, structurally identical bug in `populate_real_thread_holder`
  this fix's technique was directly copied from.
