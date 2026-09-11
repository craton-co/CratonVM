# RETIRED — FIXED 2026-09-10. The suite-only `jep290_unbounded_defaults_do_not_reject` failure was a sibling test clearing the whole of the address-keyed serialization maps

**Retired from**
`docs/known-issues/jep290-unbounded-defaults-fails-in-the-suite-and-passes-alone-20260909.md`,
written 2026-09-09. The page below is kept verbatim. Its attribution was
right — "the signature of shared mutable state, not of a wrong assertion" —
and so was its instruction to look for an ownership problem rather than a
serialization one. What it could not name is recorded here.

## The cause, in one sentence

`reset_serialization_globals()` called `.clear()` on the WHOLE of
`oos_buffers` / `ois_buffers` / `handle_registry` and the JEP-290 filter-state
map — every one of which is keyed by stream address and shared by all 4400+
tests in the binary — so the four `marshal_tests` that called it wiped the
entry a concurrently-running sibling had installed, in the window between that
sibling's write and its read.

## What the page got right, and the one thing it did not

It said to "find every test that writes the global filter and give them a
guard that restores it". A guard is exactly what `serialization_test_guard()`
already was, and the page's own "Where to look" would have found it. What the
page could not see from one failure is that the guard **had** seven callers —
the four `marshal_tests` among them — and that the problem was on the OTHER
side: the jep290 filter tests never took it. A guard only serialises the tests
that hold it.

That is why the repair is not "add the guard to ten more tests". A guard has
to be remembered by every test written afterwards, and the next one to forget
reintroduces exactly this flake. The reset is now scoped to the caller's own
stream address (`reset_serialization_state_for(addr)`), which makes one test
clearing another's state impossible rather than merely unlikely. The one
entry that genuinely is process-wide, `jdk.serialFilter`, is still cleared
globally, and every caller of the new function holds the guard while it does
so.

## Measured, both directions

Reproduced deterministically enough to count, by running the `serialization::`
subset of the lib test binary 200 times with `--test-threads=16`:

```
                                          runs failing / 200
  origin/dev (reset_serialization_globals)        60
  with the address-scoped reset                    0
```

The same loop also showed the page's severity note was an understatement: the
fault is not confined to the one test that reached a gate. Three distinct
victims appeared across the 60 failing runs, one per global map —

```
  serialization.rs:6903  jep290_unbounded_defaults_do_not_reject      filter state
  serialization.rs:6690  jep290_maxbytes_not_tripped_when_under_limit filter state
  serialization.rs:6323  m24_int_roundtrip_via_buffers                stream buffers
```

`jep290_unbounded_defaults_do_not_reject` is simply the widest window: it runs
a 1000-iteration loop between installing its filter and reading it back, so it
is the one that gets caught. Nothing about its subject — that JEP 290's
unbounded defaults do not reject — was ever wrong, which is what the page
said.

## Where the fix is

`native-builtins/src/serialization.rs`:

- `reset_serialization_globals()` → `reset_serialization_state_for(addr)`,
  which `remove`s the caller's address from each map instead of clearing it.
- `filter_state_clear_all()` deleted; it had no other caller.
- the four `marshal_tests` call sites pass the address they already declare on
  the line above.
- new test `resetting_one_stream_leaves_every_other_stream_alone`, which
  installs a sibling's buffers and filter, resets a DIFFERENT address, and
  asserts all three survive. That turns the isolation contract into something
  checked at every run instead of something a flake has to re-discover.

`cargo test -p cratonvm-native-builtins --features synthetic-jdk --lib
serialization` is 162 passed / 0 failed after the change (161 before, plus the
new test).

## Related

- The page's own pointer, `a-suite-failure-that-passes-alone-is-leakage-check-isolation-first`,
  is still the right first move: the isolation run is what made this an
  ownership question in one step.

---

*(original page follows verbatim)*

# `jep290_unbounded_defaults_do_not_reject` fails inside the 4408-test lib binary and passes alone, so a process-global serialization filter is leaking between tests

**Status:** open, found 2026-09-09. Attribution settled; mechanism not.
**Severity:** it reddens a gate arm — `cargo test -p cratonvm-native-builtins
--features synthetic-jdk --tests` — intermittently, which is worse than a
reliable red, because the next person to see it will attribute it to whatever
they happened to be changing. This page exists so they do not.

## What was seen

```text
---- serialization::serialization_tests::jep290_unbounded_defaults_do_not_reject stdout ----
thread '...' panicked at native-builtins/src/serialization.rs:6903:51:
filter state

test result: FAILED. 4408 passed; 1 failed; 7 ignored; 0 measured;
             0 filtered out; finished in 112.83s
```

## It is leakage, not a defect in the test's own subject

Run on its own, the same binary, the same tree, twice on two different runs:

```text
cargo test -p cratonvm-native-builtins --features synthetic-jdk --lib \
  serialization::serialization_tests::jep290_unbounded_defaults_do_not_reject -- --exact

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;
             4416 filtered out; finished in 0.00s
```

And it did not recur in the suite on the two runs after the one that caught it.
So: fails in company, passes alone, and does not fail in company every time —
the signature of shared mutable state, not of a wrong assertion.

`serialization.rs:6903` reads a **process-global** serialization filter. Rust
test binaries run tests in threads of ONE process, so any sibling that installs
or clears a global filter and does not restore it can strand this one. The
`filter state` panic is that read finding nothing.

## What this is NOT

**Not the 2026-09-09 `JavaLangAccess` carrier change**, which is what was in
flight when it appeared. That change touches carrier-class naming and JLA
registration; it cannot reach a serialization filter, and the isolation runs
above were taken from the tree carrying it.

**Not a missing feature.** The subject of the test — that JEP 290's unbounded
defaults do not reject — is unrelated to why it fails.

## Where to look

The fix is an ownership question, not a serialization question: find every test
that writes the global filter and give them a guard that restores it, or key the
filter per-VM the way `resolve_jla_carrier` was deliberately NOT memoised in a
process global for exactly this reason (`shared_secrets_bridge.rs`: a `OnceLock`
there "would instead pin the FIRST VM's answer for the life of the process,
which in this crate's own test binaries means every later VM inherits it").

Start by listing the writers:

```
grep -rn "set_serial_filter\|filter state\|serialFilter" native-builtins/src/serialization.rs
```

Recent commits touching that file, none of which look like the cause but all of
which are cheaper to read than a bisect: `3de6b9c64` (cargo fmt), `107efe18a`,
`ad82db066`, `b4b7d9ab3`.

## Related

- [`a-suite-failure-that-passes-alone-is-leakage-check-isolation-first`] — the
  same shape, and the reason the isolation run was the FIRST thing tried here
  rather than the last.
