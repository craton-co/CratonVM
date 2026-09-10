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
