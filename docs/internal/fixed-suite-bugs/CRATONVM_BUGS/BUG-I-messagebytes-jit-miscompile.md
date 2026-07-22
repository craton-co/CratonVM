# BUG-I — `TestMessageBytesConversion` JIT miscompile (FIXED)

> **STATUS: FIXED** (branch `fix/tomcat-jit-exc-bugs-hi`, commit 82b9bdf6).
> `TestMessageBytesConversion` now passes `OK (864 tests)` under the JIT.

**Test:** `org.apache.tomcat.util.buf.TestMessageBytesConversion`
**Symptom (before):** `Tests run: 864, Failures: 432` with JIT enabled; `OK
(864 tests)` with `CRATONVM_DISABLE_JIT=1`. HotSpot: PASS.

## Root cause

The "exactly half" was not arbitrary: **all 432 failures were the
`testConversionNull` method** (the null path); `testConversion` (non-null)
passed entirely. Bisected with `CRATONVM_JIT_BISECT_ONLY`/`CRATONVM_JIT_BISECT_SKIP`
to a single method — `MessageBytes.setString` — then minimized to a standalone
reproducer (`bench/BugI.java`):

```java
void setString(String s) {
    strValue = s;                       // putfield, value = s
    if (s == null) { type = T_NULL; }   // ← this branch was elided under JIT
    else           { type = T_STR;  }
}
```

The JIT **null-check-elimination** pass (`jit/src/null_check_elim.rs`) marks the
local feeding the `aload N` immediately before a "dereferencing" opcode as
proven non-null. `putfield` was in that opcode set — but `putfield`'s stack is
`[..., objectref, value]`, so the instruction immediately preceding it is the
stored **value** (`aload s`), never the receiver. So `strValue = s` wrongly
recorded `s` non-null, and the following `if (s == null)` was folded to its
non-null arm: a null argument set `type = T_STR`, so `isNull()` returned false
and every `testConversionNull` assertion failed.

The same flaw affected `invokevirtual`/`invokespecial`/`invokeinterface` with
arguments (the preceding push is the last argument, not the receiver):
`sink.use(arg); if (arg == null) …` elided the check too (confirmed separately).

## Fix

Remove `putfield` (0xB5) and `invoke*` (0xB6/0xB7/0xB9) from
`opcode_dereferences_receiver`, leaving only opcodes whose receiver IS the
top-of-stack operand the preceding `aload` pushed: `getfield` (0xB4),
`arraylength` (0xBE), `monitorenter`/`monitorexit` (0xC2/0xC3). (This pass has
no constant pool, so it cannot tell a 0-arg invoke — whose receiver *is* on top
— from an N-arg one; excluding all `invoke*` is the sound choice.) The change
only removes *false* non-null facts, so the JIT emits more — never fewer — null
checks: it cannot mis-elide a real check, only forgo an optimization.

Verified: `TestMessageBytesConversion` `OK (864)`; bench fib/sieve/matrix +
bintrees18 checksums match HotSpot (no correctness regression). Reproducer:
`bench/BugI.java`.
