# WP4.6 ConcurrentHashMap Boxed-Value Round-Trip Gap

## Status

Open. Default CI ignores the currently failing boxed-value CHM probes while
keeping `test_chm_clear_empty` active as a basic allocation/size/clear guard.

## Repro

```powershell
cargo test -p cratonvm-vm --test wp4_6_chm_basic -- --ignored --nocapture --test-threads=1
```

The older extended interpreter corpus shows the same family when explicitly
enabled:

```powershell
$env:CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS='1'
cargo test -p cratonvm-vm --test interpreter_tests concurrent_hashmap -- --nocapture --test-threads=1
```

## Observed Behavior

- `test_chm_pre_resize_put_get` returns `Ok(Some(Int(0)))` instead of
  `Ok(Some(Int(1)))`.
- `test_chm_mutation_cycle` returns `Ok(Some(Int(0)))` instead of
  `Ok(Some(Int(1)))`.
- The resize-heavy CHM tests are already ignored under
  `WP4.6-FOLLOWUP-A`.
- `test_chm_clear_empty` still passes and remains active.

## Notes

The failing probes use boxed `Integer` values in `ConcurrentHashMap`.
The string-only clear/isEmpty/size path still passes, which points at the
boxed-value round-trip/mutation path rather than basic CHM allocation.

`CRATONVM_DISABLE_JIT=1` does not change the result, so this is not the old
W2-CHM `Integer.valueOf` JIT miscompile.
