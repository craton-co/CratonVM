# `no_new_test_only_public_api` is one over its frozen baseline on dev

**Status: OPEN.** Found 2026-08-21 while validating an unrelated JIT branch
against `dev` at `ee4cdf528`.

```
test-only-api: 320 offenders across 2836 declarations (baseline 319)
thread 'no_new_test_only_public_api' panicked at vm/tests/no_test_only_public_api.rs:389:
test-only public API rose to 320 (baseline 319).
```

It is the only failure in `cargo test --release -p cratonvm-vm` — 3 789 tests
across 79 binaries otherwise green.

## It is dev's, and the proof is cheap

The gate is a **pure source scanner**: it walks `vm/src/**.rs` with
`std::fs::read_to_string` rooted at `CARGO_MANIFEST_DIR` and counts `pub` /
`pub(crate)` / `pub(super)` declarations whose only references are in test code.
It does not link the VM, does not run compiled code, and does not read any other
crate.

So the count is a function of the `vm/src` file contents and nothing else. The
branch that found it changes zero files under `vm/src` (its diff is
`jit/src/lib.rs`, `types/src/flag_groups.rs`, two docs and one probe), and `vm`
depends on `jit` rather than the reverse. The 320 measured on that branch **is**
`ee4cdf528`'s 320.

Corroborating: the same gate was GREEN in a full `cratonvm-vm` run on
2026-08-21 from `e708b1856`, where the only failure was
`ir_exception_stub_stamps_this_methods_throw_bci`. So the +1 arrived between
those two points.

## Where to look

`git diff e708b1856..ee4cdf528 -- vm/src` adds ten `pub` items, all of them in
the rootscan-trace work on `vm/src/jit/conservative_roots.rs`:

```
pub static FRAMES / WORDS / NEVER_MAPPED / WRONG_MAP / BELOW_JIT
pub static UNREADABLE_FRAMES / NEVER_MAPPED_WHILE_COVERED / SITES
pub fn note_site(entry_ptr: usize, off: i32, class: &'static str)
pub fn dump()
```

That is the strongest candidate for the +1 and the right place to start, but
**this page does not claim to have identified the offending item.** The gate
reports a COUNT and a full list, not a delta, and no previous list was kept, so
naming one row out of 320 would be a guess. `push_jit_entry`
(`prod=1 test=3`, same file) is in the list and looks suspicious, but its
production sibling `push_jit_entry_at` is genuinely called, and
`git log -S push_jit_entry` shows no change in that range — so it is probably a
long-standing row rather than the new one.

The cheap way to settle it: run the gate with `--nocapture` at `e708b1856` and
at `ee4cdf528` and `comm` the two sorted lists. Both runs are a source scan and
need no VM build.

## What NOT to do

The gate says it in its own failure message:

> Do NOT raise the baseline to make this pass; that is the exact failure mode
> this gate exists to stop (see ARCH-2026-08-04 A7).

The fix is either to delete the item or to make it `#[cfg(test)]` — whichever
its author intended. If a diagnostic helper like `dump()` is genuinely meant to
be called by a human through a debugger or a future flag rather than by
production code, that is a real third case, and it wants a comment saying so
plus whatever the gate's supported escape hatch is — not a bumped number.

## Related

* `ir-exception-stub-throw-bci-test-cannot-reach-its-own-tier-FIXED-20260821.md`
  — the previous red gate found the same way, by running the full suite while
  validating something else.
* `stub-ratchet-was-a-compile-error-and-is-three-over-baseline-20260820.md` —
  and the one before that. Three in a week is the pattern worth noticing: these
  gates are being found by branches that happen to run the suite, not by
  whatever runs it on `dev`.
