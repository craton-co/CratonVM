# G1 parallel evacuation: the CAS loser adopted a tagged mark word as an address — FIXED 2026-08-07

**Status:** FIXED. Retired from
`docs/known-issues/vm/g1-parallel-evacuation-torn-value-race-and-hang-20260807.md`.

**Reproducer:** `g1::tests::parallel_young_diamond_shared_children_dedup` —
**11/20 hangs** on pristine `dev`, **0/40** after.

## Root cause: one missing decode

`SharedEvac::evacuate` installs the forwarding pointer with a tagged CAS on the
mark word. A forwarded word is `target | MARK_FORWARDED` (`make_forwarded`) —
the tag lives in the low bits, and `forwarding_target` masks it back off.

The **winner** encoded correctly. The **loser** did not decode:

```rust
Err(winner) => Some((winner as *mut u8, false)),   // the whole bug
```

That hands back the raw mark word cast to a pointer, i.e. the real address **+3**
(`MARK_FORWARDED == 3`). The loser then writes that value into the parent's
reference slot and pushes it onto the gray queue. Every reported panic address
is an 8-aligned address plus three:

```text
ObjectRef pointer not 8-byte aligned: 0x7299462265db   (…5d8 + 3)
                                      0x72994622662b   (…628 + 3)
                                      0x7299462268d3   (…8d0 + 3)
```

The sibling loser arm twenty lines above — the evacuation-failure self-forward —
had always decoded properly:

```rust
Err(actual) if ObjectHeader::is_forwarded_mark(actual) => {
    Some((ObjectHeader::forwarding_target(actual), false))
}
```

so the two arms of the same function disagreed about what the CAS returns. The
fix makes the copy path identical to it, defensive check included.

**Only a two-worker race on the same object reaches that arm**, which is why it
needed a *diamond* — a child with two parents — to appear at all, and why every
tree-shaped evacuation test in the crate passed. `make_forwarded` has exactly two
call sites in the workspace, both in `g1.rs`; the other nine `compare_exchange`
sites in `gc/` are on `identity_hash_code`, a plain `AtomicI32` where returning
the loser's value verbatim is correct. The audit is closed.

**This was not just a test hang.** The bogus reference is stored into a live
object and queued for scanning. The debug assertion in `ObjectRef` is what turned
it into a visible panic; without it, it is a corrupt reference in the heap.

## Second defect: the crash became an unkillable hang

`run_worker` decremented the termination counter *after* `process_object`:

```rust
Some(addr) => {
    self.process_object(...);            // panics here
    if !children.is_empty() { ...fetch_add... }
    self.outstanding.fetch_sub(1, ...);  // skipped by the unwind
}
```

so each panicking worker leaked one count (measured: `outstanding=4 queue_len=0
wrapped=false` — a leak, not a `usize` wrap). The counter never reached zero, so
every surviving worker spun in `None => yield_now()` forever and the driver
parked in `thread::scope`, which meant the panics were **never propagated**.
Orphans on the build host reached **3h08m of CPU for a suite that finishes in
2.5s**.

Fixed with a `RetireOnExit` guard whose `Drop` does the `fetch_sub`, so the count
is retired on every exit including an unwind. The guard drops at the end of the
block — after the children are added — so the existing "add children before
retiring the parent" ordering is preserved.

## Each fix does its own job

Reverting them independently, 20 runs each:

| decode (A) | panic-safe (B) | outcome |
|---|---|---|
| broken | broken | **11/20 hangs** — `dev` as found |
| broken | fixed | 14/20 **failures**, 0 hangs — B converts the hang into a loud, diagnosable failure |
| fixed | broken | 0/20 |
| fixed | fixed | **0/40** |

B is defence-in-depth, not redundancy: with A fixed it never fires, and if
anything ever panics in a worker again the collection fails instead of wedging
the process.

## Verification

| | |
|---|---|
| `parallel_young_diamond_shared_children_dedup` | 0 failures / 40 runs (was 11/20 hangs) |
| `cargo test -p cratonvm-gc --lib` | 979 passed, 0 failed |
| full gc suite, 4 concurrent instances × 2 rounds | 8 runs, 0 failures, 0 hangs — the loop that originally caught this |
| `TestSecurity2019` under `-XX:+UseG1GC -Xmx512m` | `OK (3 tests)` |
| `TestCGIServletCmdLineArguments` under `-XX:+UseG1GC` | `OK (18 tests)` |
| `cargo check -p cratonvm-gc --all-targets` | clean, no warnings |

`cargo test -p cratonvm-vm --lib` is 2450 passed / 1 failed:
`memory::addr_keyed::tests::the_address_keyed_table_census_is_complete`, which
reports `native-builtins/src/net_phase_e.rs: census says 4 declaration(s), found
6`. That is a stale census for someone else's two new address-keyed tables (last
touched by `f53d737b6`); this branch changes one file, `gc/src/g1.rs`. Left alone
deliberately — the test exists to force an audit of those two tables, and
bumping the number without auditing them is the one thing it is designed to
prevent.

## The lesson

The two arms of a single CAS disagreed about the type of the value it returns.
`compare_exchange` hands back the *current word*, and when that word is an
encoded pointer, the failure path needs exactly the same decode the success path
needed for encoding. A raw `Err(x) => x as *mut u8` next to an
`Err(x) if is_forwarded_mark(x) => forwarding_target(x)` in the same function is
the shape to grep for.
