# H2 — interpreter operand stack underflow

## Status
**FIXED / not reproducible** (dev, 2026-06-05) — was a downstream consequence of the `toArray(T[])` ClassCast corruption (see `bug-h2-arraylist-toarray-multidim.md`); with that fixed it no longer recurs. Verified: **0** `operand stack underflow` and **0** `CRATONVM_DBG_UNDERFLOW` tripwire hits across `TestScript --nojit` and a 240 s `TestAll --nojit` run. A low-cost env-gated tripwire (`CRATONVM_DBG_UNDERFLOW=1`, dev `be18c05`) remains in `execute_frame` to name the offending `class.method desc bci opcode` in one line if it ever recurs.

## Severity
**HIGH** — VM interpreter correctness; can corrupt execution unpredictably.

## App / suite
- **App:** H2 Database (`apps/h2database`)
- **Context:** Some SQL execution paths during `TestAll`
- **Reference:** `continue_prompt_h2_testall.md`

## Symptom

```
IllegalStateException: operand stack underflow
```

During bytecode interpretation (exact test class varies).

## HotSpot behavior

No operand stack underflow — stack depth invariant holds for the same bytecode.

## CratonVM behavior

Interpreter detects stack pop when empty → `IllegalStateException`. Indicates **wrong stack depth** after one or more bytecode instructions (wrong wide/narrow encoding, bad exception handler edge, or incorrect fast path).

## Root cause (suspected)

Bytecode execution bug in CratonVM interpreter (or EC JIT if enabled):

- Incorrect stack delta for specific opcode(s) hit in H2’s SQL engine
- Exception/unwind path not restoring stack correctly
- `dup`/`swap`/category-2 value handling

## Impact

Unpredictable failures in H2 SQL engine; may manifest as generic SQL errors or crashes deep in tests.

## Reproduce

Run full `TestAll` after SHA1PRNG fix; grep:

```bash
grep -i "operand stack underflow" h2-testall.log
```

Use `CRATONVM_DBG` / stack logging if available to capture failing method + BCI.

## What to fix

1. Capture opcode trace at failure site (method, BCI, stack depth).
2. Compare against JVM spec stack map for that instruction.
3. Fix interpreter (and JIT if applicable) stack accounting.
4. Add micro-test for the offending bytecode sequence.

## Related

- [bug-h2-prepared-statement-column-count.md](bug-h2-prepared-statement-column-count.md) (may share JDBC/SQL interpreter paths)
