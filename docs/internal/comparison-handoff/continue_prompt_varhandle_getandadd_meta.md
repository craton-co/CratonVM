# VarHandle.getAndAdd (and siblings) ignore the meta side-table → wrong on real-JDK VarHandles

**Severity:** medium (latent correctness bug; surfaces as silently-wrong atomic updates / NoSuchMethod-adjacent failures on real-JDK VarHandles). Self-contained. Baseline commit: `d6cefc7` on `dev`.

## Background
CratonVM represents most VarHandles with a synthetic 6-field layout (`VH_KIND`, `VH_FIELD_INDEX`, `VH_CLASS`, `VH_FIELD`, …). But **real-JDK VarHandles** created by genuine `MethodHandles.lookup().findVarHandle(...)` bytecode do NOT carry that layout — their per-handle metadata lives in a side table reachable via `vh_meta_get(ctx, vh)` (`native-builtins/src/lang_invoke.rs`, returns `Arc<VarHandleMeta>` with `.kind`, `.field_index`, `.class_name`, `.field_name`).

`java.net.Socket.STATE` is exactly such a real VarHandle. The recently-added `varhandle_get_and_bitwise` (commit `d6cefc7`) handles this correctly by calling `vh_meta_get` FIRST and falling back to the synthetic fields — mirroring `varhandle_compare_and_set`. But the pre-existing `varhandle_get_and_add` (`lang_invoke.rs` ~line 1234) reads `VH_KIND` straight off the receiver object with **no `vh_meta_get`** — so for a real-JDK VarHandle it mis-resolves kind/field and returns `Ok(Some(Value::Int(0)))` without performing the update (confirmed by adversarial review).

## Repro
Any class that uses `VarHandle.getAndAdd` on an instance/static field via a real `findVarHandle` (not the synthetic path). Examples: a `LongAdder`-style accumulator, or a hand-rolled `STATE.getAndAdd(this, n)` int-field counter. Symptom: the field is never actually updated (atomic getAndAdd is a no-op returning 0). Build a minimal probe like `C:/tmp/audit/SockProbe2.java` was for `getAndBitwiseOr`: a class with `private volatile int n;` + `static final VarHandle N = MethodHandles.lookup().findVarHandle(Cls.class,"n",int.class);` and assert `N.getAndAdd(obj, 5)` returns old and leaves `n == old+5`.

## Fix
Apply the SAME `vh_meta_get`-first resolution that `varhandle_get_and_bitwise` and `varhandle_compare_and_set` use, to `varhandle_get_and_add`:
```rust
let meta = vh_meta_get(ctx, this);
let (kind, field_idx) = match meta.as_deref() {
    Some(m) => (m.kind, m.field_index),
    None => { /* read VH_KIND / VH_FIELD_INDEX off the object as today */ }
};
// instance/static branches: resolve class/field from meta first, else VH_CLASS/VH_FIELD strings.
```
Use `varhandle_get_and_bitwise` (in the same file) as the reference — copy its meta resolution block verbatim, keep `get_and_add`'s `add_values` arithmetic.

## Audit the other VarHandle ops for the same latent bug
Grep `lang_invoke.rs` for every signature-polymorphic VarHandle native that reads `ctx.get_field(this, VH_KIND)` WITHOUT a preceding `vh_meta_get`. Known meta-aware (good): `varhandle_compare_and_set`, `varhandle_get_and_bitwise`. Check at minimum: `varhandle_get_and_set`, `varhandle_compare_and_exchange`, the plain `get`/`set` handlers, and any array/static variants. Fix each that's missing the meta lookup. (The fix is mechanical and identical.)

## Verification
- New probe: real-VarHandle `getAndAdd` updates the field and returns the old value (gate-independent — this is not behind `CRATONVM_REAL_NET_SOCKETS`).
- Re-confirm `getAndBitwiseOr` still works: `CRATONVM_REAL_NET_SOCKETS=1 cratonvm.exe -cp C:/tmp/audit SockProbe` → PASS.
- Add a unit test in `lang_invoke.rs` tests if feasible (mock a meta entry).
- Regression pool stays 13/14.

## Key files
`native-builtins/src/lang_invoke.rs` (`varhandle_get_and_add` ~1234; reference: `varhandle_get_and_bitwise`, `varhandle_compare_and_set` ~1030; `vh_meta_get` ~201). `vm/src/vm/vm_exec.rs` (signature-polymorphic method-name list ~8684 — already covers getAndAdd/getAndBitwise; no change expected). Memory: `reference_server_socket_gap`.

## Build/test gotchas (Windows)
Before each rebuild: `taskkill //F //IM cratonvm.exe cargo.exe rustc.exe`; `rm -f target/release/cratonvm.exe`; verify exe mtime advanced; ONE build at a time. See memory `reference_windows_exe_lock_build_trap`.
