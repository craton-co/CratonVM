# `no_new_test_only_public_api`: the gate was miscounting, in both directions — FIXED 2026-09-02

**Status: RESOLVED.** Gate green, `4 passed; 0 failed`, **with the baseline
unchanged at 318**. Opened 2026-08-21 at 320/319; the numbers moved several
times before this, so read the mechanism rather than the counts.

## The page's guess was wrong, and the right method was cheap

The original page named the rootscan-trace `pub` items in
`jit/conservative_roots.rs` as "the strongest candidate for the +1", and was
careful to say it had not identified the offender. Good instinct: **none of
them is on the list today**, and the cause was not a new `pub` item at all.

Its prescribed method — run the gate at two commits and `comm` the lists — is
right and cheaper than it looks. The gate reads `vm/src/**.rs` **at runtime**
from a `CARGO_MANIFEST_DIR` baked in at build time, so ONE prebuilt binary
scores any revision: `git checkout <rev> -- vm/src`, run, restore. No rebuild
per revision.

> Restore with `git checkout HEAD -- vm/src`. `git checkout <rev> -- vm/src`
> STAGES the old content, so a plain `git checkout -- vm/src` restores from the
> polluted index and silently leaves the worktree on the old revision — it left
> 67 modified paths in a landing worktree here.

Scored that way, `vm/src` at the commit that froze the baseline and `vm/src` at
HEAD both produced **319**. The +1 was never in the source. It was in the
scanner.

## The defect: a `format!` escape is not an opened block

`split_regions` tracks `#[cfg(test)]` regions with `brace_delta`, which counted
`{` and `}` on the **raw line** — including braces inside string literals.

`vm/src/vm/vm_exec.rs:31238` is

```rust
format!("if pdesc == \"J\" {{ if let Some(obj) = {call}(...)
```

`{{` is a format ESCAPE — a literal brace in the output, not a block. Counted
raw it leaves the region at depth 2 forever, so **every later line of the file
is filed as test**.

Measured across `vm/src`: **five files, 5 131 production lines** swallowed.

| file | lines swallowed | from |
| --- | --- | --- |
| `vm/src/vm/vm_exec.rs` | 3 633 | 29 748 |
| `vm/src/vm/vm_object.rs` | 541 | 2 202 |
| `vm/src/vm/vm_init.rs` | 386 | 17 633 |
| `vm/src/bin/bench_gate.rs` | 300 | 592 |
| `vm/src/runtime/env_cache.rs` | 271 | 2 306 |

This is the SECOND incarnation of the bug `fb9381fa4` fixed — that one was
`pending` latching on a brace-less gated item ("blind to 7 732 lines"), this one
is the brace count itself.

### It was wrong in BOTH directions, which is why the count barely moved

A stray brace can also close a region EARLY, filing test lines as production.
In `vm_init.rs`, **6 073 lines** change classification when the count is fixed.
So the gate was simultaneously hiding real offenders and inventing false ones,
and the two nearly cancelled — which is exactly why a wrong count of 319 looked
stable enough to chase as "one new `pub` item".

The visible symptom was `with_cas_lock` reported `prod=1` (declaration only)
while `vm_exec.rs:33264` calls it in production.

## What the fix found

`brace_delta` now ignores braces inside string literals, char literals and line
comments. Every one of the 142 files then closes its regions.

* **1 false offender removed**: `with_cas_lock` — it has a production caller.
* **3 true offenders revealed**, each with only `#[test]` callers, previously
  hidden inside a swallowed region: `drain_cleaners` (`vm_init.rs`),
  `scan_class_natives` (`vm_object.rs`), `process_command`
  (`serviceability.rs`).
* Gating those exposed a fourth by cascade — `dispatch_command` — because
  gating a function moves its BODY into the test region too, so its callees can
  become test-only in turn.

Four `#[cfg(test)]` gates later the count settles at **318 — the baseline that
was already frozen.** Nothing was raised, and nothing needed lowering.

## The one that nearly got gated for a wrong reason

`process_command` looked like the doc's "third case" — a diagnostic surface
meant to be driven from outside the process. The evidence for that was strong:
`Vm::new` constructs the processor deliberately ("the real attach-API jcmd
processor", "register the *live* jcmd command set"), and
`AttachListener::start_listening` really does `bind` a Unix socket at
`/tmp/.java_pid<pid>` with 0600 permissions and spawn a polling accept thread.

I had written an `EXTERNALLY_DRIVEN` escape hatch for it — the mechanism this
gate's own failure message asks for and does not have — and then checked the
one thing that decides it. **The accept loop dispatches through
`handle_attach_connection`, which does its own
`.find(|c| c.name == name).map(|c| c.execute(&args))` rather than calling
`process_command` or `dispatch_command`.** The jcmd client never reaches either
wrapper. Both are test-only, the hatch's premise was refuted, and it was
deleted rather than shipped.

**The gate still has no escape hatch for a genuine third case.** Nothing here
needed one, so none was invented; a mechanism justified by a reason that turned
out to be false is worse than no mechanism.

## A stale header corrected on the way

`serviceability.rs`'s LIVENESS block says no attach socket is ever created and
`JcmdProcessor` is constructed only in `#[cfg(test)]` code. Both are false, and
by same-day work: `obsaudit D15` (2026-07-26) wired it up. The header is dated
2026-07-26 too — it describes the state its own day's later commit changed.
Corrected in place, narrowed to what is still true.

## What to carry

**A source-scanning gate is a parser, and a parser that is wrong is wrong in
both directions at once.** The failure mode is not a wrong number — it is a
number that looks stable while the membership underneath it churns. Two of the
three "new offenders" this page chased across two weeks never existed, and the
one real problem was never in `vm/src` at all.
