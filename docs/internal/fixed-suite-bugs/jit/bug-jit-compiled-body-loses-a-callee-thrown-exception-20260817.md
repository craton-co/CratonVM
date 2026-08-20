# JIT: a compiled body lost a callee-thrown exception, and the caller read it as success — FIXED

## Status
**FIXED 2026-08-17** on `fix/jit-exception-loss-20260817`, pinned by
`vm/tests/resources/cratonvm/JitCalleeExceptionShapes.java` (two checksums, both
golden against HotSpot 25, both red before the change) and by the two bc-java
suites that found it.

| suite | before | after |
|---|---|---|
| `jce.provider.test.nist` (286 PKITS vectors) | 37 errors | **0 — class passes** |
| `crypto.test` `CipherStreamTest` | fails on a tamper vector | **Okay** |

Both symptoms were silent. A `CipherInputStream` over TAMPERED AEAD ciphertext
read to a clean EOF, and a PKIX revocation check was skipped. Nothing logged,
nothing threw, and the caller's success path ran.

## The defect

The routing could not tell **"this method does not catch this throw"** from
**"the throw site is unknown"**, and it treated the first as the second. Two
halves, one cause.

### 1. An out-of-range stamp was reported as "unknown"

A compiled body stamps its own throw-site bci (`set_throw_bci`, one pad per
distinct throw site) before returning the deopt sentinel.
`jit_local_athrow_pc` then validated that stamp against the method's own
protected ranges — the filter that rejects a FOREIGN bci — and answered
`usize::MAX` ("unknown") whenever it landed outside all of them.

But "outside every `try`" is not unknown. It is the method saying it cannot
catch the throw. Reported as unknown, it reached the pc-UNKNOWN handler search,
which matches typed rows **by exception class alone** — so a
`catch (RuntimeException)` guarding `[23,27)` "caught" a throw at bci 20 and the
method returned normally.

`jit_local_athrow_pc_kind` now answers three states — `InRange(pc)` /
`OutsideAllRanges` / `Unknown` — and `OutsideAllRanges` propagates without
searching. `jit_local_athrow_pc` is kept as a two-state wrapper for the callers
that only want a pc.

### 2. A callee that could not catch was RE-EXECUTED from its entry

`route_implicit_exc_through_callee` re-ran the callee in the interpreter
whenever it could not resume that callee's own handler — a fallback written for
a real problem (the JIT cannot dispatch to a compiled callee's in-method
handler) but applied to a case it does not fit.

For a callee whose pre-throw prefix is not idempotent, the re-run does not
merely duplicate work. BouncyCastle's `CipherInputStream.nextChunk`:

```java
if (finalized) return -1;          // the early exit the re-run takes
...
finaliseCipher();                  // sets finalized = true, THEN throws
```

so the second pass returned `-1` and the exception disappeared.

`run_jit_callee_handler` now reports **why** it did not resume —
`CalleeHandlerMiss::NotCaught` vs `::Declined` — and a genuine `NotCaught` with
a known throw pc propagates to the caller, which is what the JVM does.

**The re-run is deliberately kept** for the case it was written for: with no
usable stamp, "no handler" cannot be trusted, because the pc-unknown search also
skips a catch-all whose region does not span the whole method — i.e. every javac
`finally`. bc-java's `SymmetricConstraintsTest` restores a PROCESS-WIDE
`CryptoServicesRegistrar` constraint in exactly such a block; losing it fails
every later `HPKETestVectors` case with "service does not provide 192 bits of
security".

## How it was found, and what did not work

`CRATONVM_JIT_DENY` (a substring match on `class/name.method`) bisected each
cluster to one method in four runs, with `--nojit` as the control:

```bash
CRATONVM_JIT_DENY=org/bouncycastle/crypto/io/CipherInputStream.nextChunk   # Okay
CRATONVM_JIT_DENY=org/bouncycastle/jce/provider/ProvRevocationChecker.check # 0 of 208 fail
```

Ruled out, one run each: GC (`CRATONVM_DBG_GC_STRESS`), heap size, C2
(`CRATONVM_JIT_NO_EXC_TABLE_C2`), side-table collisions (identity hashes are a
monotonic counter, so they never recycle), and repetition (the same vector 40x
in one JVM passes 40x — what accumulates is how many DISTINCT methods get hot).

**Two instruments lied and are worth remembering.**

* An instrumented **classpath shadow** of `ProvRevocationChecker` — the same
  `.java` with two `System.err.println` calls, compiled into a directory placed
  first on the classpath — made all 208 vectors pass. The bigger method compiles
  differently. Use the shadow to read the SEMANTICS (it proved `PREFER_CRLS` and
  `NO_FALLBACK` are both false on every call, so the CRL fallback should always
  have run) and take the verdict from the `CRATONVM_JIT_DENY` A/B.
* The obvious **synthetic probes were green**: two disjoint try ranges catching
  one type with a throw from the second, and a loop calling a throwing method
  outside the try. What finally reproduced it was neither — it was the shape
  with a LATCH in the prefix (`JitCalleeExceptionShapes.outsideTryStep`), and
  `CRATONVM_DBG_RBC6=1` naming `handler_pc=Some(28)` for a throw at bci 20 is
  what turned a hypothesis into a measurement.

## What this did not fix

`crypto.test` still fails, on a different and pre-existing defect the fix
EXPOSED: `SimpleTestTest` reports only its first failing entry, so with
`CipherStreamTest` (index 130) green the run now reaches
`SymmetricConstraintsTest` (index 179), which fails with "no exception!" on
`--nojit` and on the pre-fix binary alike, and leaks the process-wide constraint
that then fails 14 `HPKETestVectors` cases. That belongs to the bc-java residual
page, not here.
