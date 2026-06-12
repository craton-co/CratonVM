# Bug 03 — Rust panic (operand-stack index OOB) in `org.apache.kafka.common.utils`

**Severity:** High — `org.apache.kafka.common.utils` aborts (process panic).
`--nojit`, so interpreter-path. HotSpot runs the package clean.

**Symptom:**
```
thread 'main-vm' panicked at vm\src\runtime\value_stack.rs:796:19:
panic: index out of bounds: the len is 24 but the index is 24
```

The interpreter's operand-stack access at `value_stack.rs:796` indexes element
`24` of a 24-element stack (off-by-one / under-provisioned `max_stack`). This is a
VM correctness bug — a panic is never an acceptable outcome for valid bytecode
that HotSpot accepts.

## Next steps
- Identify the bytecode/method that drives the stack to depth 24 with `max_stack`
  computed as 24 (likely a verifier/`max_stack` mismatch or a stack-growth path
  that bypasses the bounds reservation).
- Determine whether `value_stack.rs:796` should grow the backing store or whether
  `max_stack` is being mis-read for the offending method.

## Status
- [x] Reproduced (package `common.utils`, `--nojit`).
- [ ] Root cause / fix (open).
