# T11 safety-annotation coverage thresholds

**Status:** FIXED on 2026-07-01.

**Resolved by:** documenting the remaining `../../../jit/src/x64.rs` leak/ownership
sites that held raw pointers into emitted JIT test code. The final failing gate
was `t11_4_jit_leaks_documented` at 18/23 documented leak patterns (78%), below
the 80% threshold. After the fix, the gate reports 23/23 (100%).

## Verification

```bash
cargo test -p cratonvm-vm --test t11_safety_conformance -- --nocapture
```

Result: 15/15 passed.

## Final Coverage

| Check | Result |
|-------|--------|
| T11.1 `../../../gc/src/gen_heap.rs` unsafe blocks | 166/172 (96%) |
| T11.1 `../../../jit/src/x64.rs` unsafe blocks | 233/251 (92%) |
| T11.1 `../../../vm/src/jit/helpers.rs` unsafe blocks | 66/70 (94%) |
| T11.1 `../../../vm/src/runtime/interpreter.rs` unsafe blocks | 66/67 (98%) |
| T11.3 casts | 1615/1712 (94%) |
| T11.4 `../../../jit/src/x64.rs` leak patterns | 23/23 (100%) |
| T11.5 `vm_init` bare unwraps | 0 |
| T11.6 `lock_order.rs` | present |
| T11.7 TODO/unsafe sentinels | clean |

## Fix

Added concrete `// LEAK(intentional):` comments for:

- The per-site OSR trigger counter whose address is baked into emitted test code.
- A scalar-replacement test `JitInvokeInfo` block and its string fields, all of
  which are referenced by raw pointer from generated code and therefore must
  outlive the compiled method.

No runtime behavior changed.
