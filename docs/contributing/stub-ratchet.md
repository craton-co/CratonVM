# Synthetic-stub ratchet

Run:

```text
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
```

The baseline equals the exact default-registry count. There is no slack.

- If a change removes stubs, lower `BASELINE_SYNTHETIC_STUBS` to the printed
  count in the same commit.
- If it adds one, implement the behavior as real bytecode, a Bridge, or an
  Intrinsic. Raising the baseline requires an explicit design explanation and
  a tracked removal issue.
- A zero-registration registry is rejected separately so broken census wiring
  cannot pass vacuously.
