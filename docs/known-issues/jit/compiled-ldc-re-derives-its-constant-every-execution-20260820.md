# Compiled `ldc` re-derives its constant on every execution — the JIT twin of a bug fixed in the interpreter on 2026-08-18

**Status: OPEN.** Found 2026-08-20 while profiling
`httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`'s snappy phase, where the
interned-string pool is ~5 % of samples on a workload that shuffles bytes.

JVMS §5.4.3: a resolved constant-pool entry returns the same result on every
later resolution of it. The interpreter was taught that on 2026-08-18
(`ldc` re-derived its constant every execution — FIXED), and the store it now
uses is collector-remapped, redefinition-invalidated, loader-aware and bounded.

**Compiled code never adopted it.** Both JIT `ldc` helpers still do the full
derivation per execution:

```rust
pub extern "C" fn jit_ldc_string(vm_ptr: i64, bytes: *const u8, len: usize) -> i64 {
    …
    crate::vm::create_java_string(shared, text).as_ptr() as i64   // pool lock + content hash
}

pub unsafe extern "C" fn jit_ldc_class_cp(vm_ptr: i64, holder_class_id: i64, cp_idx: i64) -> i64 {
    …
    let target_id = jit_resolve_cp_class(vm, holder_cid, cp_idx as u16, false)?;
    crate::vm::get_or_create_class_mirror(vm, target_id).as_ptr() as i64  // full resolve + mirror lookup
}
```

`jit_ldc_string`'s own doc comment states the per-execution consult as the
design ("this helper consults the VM string pool on every execution"), and
gives the reason: an immediate object address baked into generated code would
be stale after the next moving collection. That reason is real and it is also
exactly what the interpreter's store solves — it holds **global-root handles**,
not `ObjectRef`s, precisely so the collector owns the reference.

## The cost

`probes/LdcConstCostProbe.java` — each kernel runs 16 copies of ONE opcode per
iteration and is differenced against an otherwise identical loop with 16 fewer,
so the number is the marginal cost of the opcode. The loops are hot enough to
be compiled, so this prices the COMPILED arm.

| opcode | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| `ldc "literal"` | 0.2 ns | **18.4 ns** | 92x |
| `ldc SomeClass.class` | 0.2 ns | **62.7 ns** | 313x |
| *control* `iadd` | 0.2 ns | 0.4 ns | 2x |

The `iadd` control does not move, which is what says the two rows are the
opcodes and not the host.

## Where it was noticed

`NettyZipBombPhases snappy 4` (the workload behind
`httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`), `perf record`, G1, binds on:

| self % | symbol |
|---:|---|
| 2.42–4.09 | `vm_object::create_java_string` |
| 1.44 | `__memcmp_evex_movbe` |
| 1.10 | `HashMap<String, ObjectRef>::get::<str>` |

The three are one cluster: `create_java_string`'s pool is
`RwLock<FxHashMap<String, ObjectRef>>`, so a hit is a read lock, a hash of the
literal's whole content, and a `memcmp` against the stored key. Nothing else on
that path looks up a `String` by content. netty puts literals on its hottest
paths by construction — `ObjectUtil.checkPositive(increment, "increment")` sits
inside `RefCnt`'s `retain0`/`release0`, which run per buffer retain/release.

**Caller attribution is by elimination, not by a call graph.** A frame-pointer
profile shows `create_java_string` with JIT-compiled callers; a DWARF profile
of the same phase attributes it to an inline chain ending in
`decode_dispatch_values_into`, which contains no such call in source. The
cluster's identity (a by-content `String` lookup) is what pins it to `ldc`,
and the microbenchmark above is what prices it. Anyone acting on this should
add a counter to `jit_ldc_string` first and read it, rather than trusting
either profile's chain.

## The fix, and the asymmetry between the two helpers

* **`jit_ldc_class_cp` needs no ABI change at all.** It already receives
  `(holder_class_id, cp_idx)` — the recorded-constant store's exact key. The
  probe goes at the top of the helper, the same place the interpreter fix put
  it in `execute_ldc`.
* **`jit_ldc_string` does need one.** The JIT bakes the literal's BYTES and
  length, not its constant-pool index, so the helper has no key to probe with.
  The additive shape is a `ldc_string_cp` helper beside the existing slot (the
  emitters already choose a CP-indexed form for `Class`), leaving `ldc_string`
  for callers with no resolver.

Both must keep what the interpreter fix keeps: a **failed** resolution is not
recorded, and the value stored is a global-root handle rather than a raw
`ObjectRef`.

## Not to be confused with

* `interpreter-ldc-re-derived-its-constant-and-never-interned-the-wide-literals-FIXED-20260818.md`
  — the interpreter half. Fixed; this page is the compiled half, which that
  fix did not reach.
* The surrogate-literal interning half of that page is **not** duplicated here:
  the JIT's string `ldc` goes through `create_java_string`, which pools on the
  Rust `String`, so a lone-surrogate literal cannot reach it at all. Whether
  compiled code can even `ldc` such a literal is not established here.

## Related

* `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` — where
  the cluster was measured, and one of the residuals it re-homes.
