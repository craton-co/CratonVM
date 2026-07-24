# BUG-05 — `this == null` inside a varargs constructor — FIXED

| | |
|---|---|
| **Symptom** | `NullPointerException: Cannot assign field "scripts" because "this" is null` inside `ResourceDatabasePopulator.<init>` |
| **Origin** | Spring `org.springframework.jdbc.datasource.init.ResourceDatabasePopulatorTests` (6 ctor tests) |
| **Root cause** | JIT escape analysis (`analyze_escapes`) lost the `new`-receiver's provenance across an unmodeled `anewarray`, so the varargs ctor's receiver was wrongly scalar-replaced to a dummy null |
| **Fix** | `../../../jit/src/x64.rs` — the `analyze_escapes` catch-all `_` arm now ESCAPES tracked operand-stack objects instead of silently forgetting them |
| **Status** | FIXED — `ResourceDatabasePopulatorTests` 10/10 (was 7/10), matches HotSpot JDK 25 |

## Diagnosis

`new C(a, b)` for a varargs ctor `C(E... xs)` compiles to:

```
new C; dup; iconst_2; anewarray E;
dup; iconst_0; <push a>; aastore;
dup; iconst_1; <push b>; aastore;
invokespecial C.<init>([E;)V
```

The dup'd receiver sits on the operand stack **below** the array while the
varargs array is built. `analyze_escapes` (`../../../jit/src/x64.rs`) is a single linear
forward pass with an abstract operand stack tracking `new`-object provenance.

It models `new`, `dup`, `aastore`, `invokespecial`, loads/stores, etc., but
**not** `anewarray` (0xbd) — that opcode fell to the catch-all `_` arm, which
**forgot** the provenance of every stack slot (`*slot = None`). That wiped the
dup'd receiver's `Some(new_pc)` tag. The later arg-bearing
`invokespecial C.<init>([E;)V` then escaped *nothing* (the slots it popped were
all `None`), so the `new C` at the bottom was reported **non-escaping**.

Non-escaping ⇒ scalar replacement: the `0xbb` codegen arm pushes a dummy zero
"object reference" instead of allocating (the object's fields would live in the
JIT frame). But the constructor is a **real, un-inlined dispatch** — so the
dummy null flowed in as `this`, and the first `putfield this.scripts = …`
inside `<init>` saw a null receiver → the NPE.

### Localization trail

- `CRATONVM_DISABLE_JIT=1` → 10/10 pass (interpreter correct) ⇒ JIT codegen bug.
- `CRATONVM_DBG_JITC=1` → the caller (`constructWithMultipleResources()`) is
  eagerly compiled; `ResourceDatabasePopulator.<init>` stays interpreted ⇒ the
  *caller* passes a null receiver.
- `CRATONVM_DBG_JIT_DISASM=…` → at `bc@0` (`new`) the compiled caller emitted
  **no allocation**; `bc@3` (`dup`) read a slot left at zero ⇒ the `new` was
  scalar-replaced (the `0xbb` "dummy zero" path).

The eager single-pass first-call compile path (`execute()` in `interpreter.rs`)
is what exposed it on the reported build (its `bg_compile` default was off).
On current dev `bg_compile` is default-ON, so one-shot test methods interpret
and the symptom is dormant there — but the buggy `analyze_escapes` is the
**authoritative** escape pass for *every* compile (eager and the off-thread
worker via `compile_with_param_slots`), so any hot varargs-ctor caller hits it.
Proven by the unit test below (fails on the pre-fix catch-all).

## Fix

`analyze_escapes` catch-all `_` arm: before forgetting provenance, escape every
tracked object still on the operand stack. This is purely soundness-restoring —
escaping can only cause MORE objects to be heap-allocated normally, never fewer,
so it can never turn a correctly-allocated object into a (miscompiled)
scalar-replaced one. Straight-line allocation sites built only from modeled
opcodes are unaffected (the common scalar-replacement case still fires).

## Verification

- New unit test `test_escape_analysis_varargs_ctor_receiver_escapes`
  (`../../../jit/src/x64.rs`): asserts the varargs-ctor receiver escapes. FAILS on the
  pre-fix catch-all, PASSES with the fix.
- `cargo test -p cratonvm-jit --lib` → 851 passed / 0 failed.
- `ResourceDatabasePopulatorTests` → 10/10 (was 7/10), == HotSpot; both default
  and `CRATONVM_BG_COMPILE=0` (forced eager) paths.
- `CompositeDatabasePopulatorTests` 5/5. (`H2`/`Hsql` `*PopulatorTests` residual
  failures are real DB-engine `ScriptStatement`/`UncategorizedScript`
  exceptions — pre-existing, unrelated to this change.)

Minimal repros: `Vrepro3.java` (reflective eager path), `Vrepro4.java`
(hot-loop worker-compile path).
