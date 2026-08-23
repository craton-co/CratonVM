# ✅ FIXED — the HAETAE defect: the inline splicer left a 64-bit product in an `int` slot

## Status

**RESOLVED 2026-08-22** on `fix/bcjava-pqc-and-cipherstream-20260822`, one line of
codegen in `jit/src/x64/inlining.rs`.

Filed and localised over two earlier passes as *"the C1 full compile of
`HAETAEEngine.ntt`"*. The localisation was one frame off, and that is why it
survived: `ntt` is compiled correctly. What was wrong is the copy of
`montgomeryReduce` spliced **into** it.

| | before | after |
|---|---|---|
| `pqc.crypto.test.HAETAETest` | `Tests run: 1, Failures: 1` (4 s) | **`OK (1 test)`** (19 s) |
| same, second run | **`rc=124`, killed at 900 s** | **`OK (1 test)`** (13 s) |
| `probes/NttRealProbe`, modes 2 / 3 / 5 | `diverged=4498 firstBad=502` | **`diverged=0`** on all three |

## The defect

`jit/src/x64/inlining.rs` emits the spliced callee's bytecode itself. Its
`iadd`, `isub` and `imul` arms computed in **64 bits** and never narrowed:

```rust
// imul
0x68 => {
    self.pop_to_rax();
    let slot2 = self.pop_stack();
    self.load_slot_to_reg(RCX, slot2);
    // IMUL RAX, RCX
    self.rex_w();                              // <- REX.W: 64-bit
    self.buf.emit(&[0x0F, 0xAF, 0xC1]);        // <- and no MOVSXD after it
    self.push_from_rax();
    cpc += 1;
}
```

Every other `int` op in the same `match` already does the right thing —
`ineg`, `ishl`, `ishr` and `iushr` all follow their 32-bit form with
`MOVSXD RAX, EAX`. These three were the outliers.

It matters because **an `int` slot in this frame is a sign-extended 64-bit
value, and every consumer relies on that**: `i2l` is a no-op on one, `lmul` and
`if_icmp*` read the whole register, an `iastore` takes the low word. A 64-bit
`ADD`/`SUB` preserves the invariant right up until the 32-bit result overflows.
A 64-bit `IMUL` breaks it for ordinary inputs, because a 32-bit product
overflows all the time.

## Why HAETAE, and why nothing else

BouncyCastle reduces with

```java
private static int montgomeryReduce(long a) {
    int t = (int) a * QINV;              // QINV = 0x380F0401
    long tt = a - ((long) t * Q);        // Q = 0xFC01
    return (int) (tt >> 32);
}
```

The `int` multiply is **deliberately** allowed to overflow — that truncation is
the algorithm, and `QINV` is chosen so it is. Spliced into `HAETAEEngine.ntt`:

```text
286: movsxd rax,eax                     ; (int) a
290: mov    rax,380F0401h               ; QINV
2ac: imul   rax,rcx                     ; 64-BIT product, full width
2b0: mov    [rbp-0C0h],rax              ; ... straight into t's slot
2d0: mov    rax,[rbp-88h]               ; (long) t  -- a no-op, as designed
2fa: imul   rax,rcx                     ; t * Q, from a number 2^32 too large
```

so `tt` was wrong, `tt >> 32` was wrong, and every value the transform produced
afterwards was unreduced: `-827016358` where the answer is `-27922`.

Both faces of the reported defect follow from that one line:

* **KAT vector 5 verifies false.** Signing produces the KAT's bytes exactly and
  verification then rejects them — because signing and verifying take different
  paths through `ntt`, and only one of them had crossed the compile threshold.
* **KAT vector 6 never terminates.** HAETAE's rejection sampler loops until a
  sample falls in range. Fed unreduced values, it never accepts. A 240 s cap,
  then a 900 s cap, then a 3600 s cap, all `rc=124`, against HotSpot's 8 ms.

## Why the earlier localisation stopped where it did

The previous pass did good work and got to within one frame:

* `CRATONVM_JIT_DENY` matches `"{class}.{method}"`, so a binary search over
  `HAETAEEngine`'s 147 methods found `ntt` in eight runs, and denying it alone
  turned the KAT green.
* The tier knobs then said C1, not C2, not OSR, not the interpreter.
* All 53 off-switchable JIT passes were swept individually. **None fixed it** —
  correctly, because the bug is not in a pass.
* Two standalone reconstructions of `ntt`'s loop shape were written and both
  **passed**, which was read as "something about the real method's context is
  required".

Every one of those results is consistent with the real cause, and none of them
points at it, because they all treat `ntt` as the unit. The thing that moved it
was asking a different question: `CRATONVM_DBG=jit-disasm` shows **two** bodies
for `ntt` in one run — an OSR body and a full body — and

```text
control            : full ntt + full polyNtt compiled   -> FAIL
deny polyNtt       : only the OSR ntt body compiled     -> PASS
```

The OSR body does not inline `montgomeryReduce` (no `380F0401` constant appears
in it); the full body does. Two bodies of one method at one tier, one right and
one wrong, in a single log — that is the differential that names the splicer.
The standalone reconstructions passed because neither of them inlined a multiply
that overflows.

## Reproduce

`probes/NttRealProbe.java` needs no oracle and no second VM: every iteration
hands `polyNtt` a freshly built array with identical contents, so every
iteration must return an identical result, and iteration 0 defines the answer.

```bash
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "<probe-dir>:$(cat /data/bcjca-classpath.txt)" \
    org.bouncycastle.pqc.crypto.haetae.NttRealProbe 5000 2
```

**It is load-sensitive, and a green means nothing without checking why.** The
failure needs the FULL compile of `ntt`, which only happens when `polyNtt` is
compiled too. On an idle host the OSR body serves every call and the probe
passes on the *unfixed* binary — measured, three runs out of three, on the same
md5 that had failed six times an hour earlier under load average 20. Before
believing a pass, check the run actually compiled the full body:

```bash
CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=1 … | grep 'full .*HAETAEEngine.ntt('
```

`HAETAETest` is the reliable arm: it reproduced on the unfixed binary on an idle
host, both faces, in the same pair of runs the table at the top records.

## Blast radius

Strictly more correct: the change narrows three `int` operations to the width
the JVM specification gives them. It cannot turn a right answer wrong, and it
costs one 3-byte `MOVSXD` per spliced `iadd`/`isub`/`imul`.

Regression run on the same worktree:

| suite | result |
|---|---|
| `cargo test -p cratonvm-jit --lib` | 2102 passed, 0 failed |
| `cargo test -p cratonvm-native-builtins --lib` | 4148 passed, 0 failed |
| `cargo test -p cratonvm-vm --lib` | 2594 passed, **1 failed** — `enforcement_dial_door_tests::every_force_native_file_asks_the_dial_or_is_exempt`, a stale `FORCE_SITES_EXEMPT` row for `dispatch_virtual.rs`. Pre-existing on `dev`; nothing in this change touches `vm/src`. |

## The transferable part

**"The compile of method M is wrong" is a claim about a frame, not a method.**
Every lever in the earlier investigation — the deny bisect, the tier knobs, the
53-pass sweep — was aimed at `ntt`, and `ntt`'s own codegen was correct
throughout. The bug was in a callee that no longer existed as a callee. When a
per-method bisect lands on a method whose code you cannot fault, ask what got
spliced INTO it.

**A shipped invariant with four honourers and three violators is not an
invariant.** `ineg`, `ishl`, `ishr` and `iushr` narrowed; `iadd`, `isub` and
`imul` did not, in the same `match` statement, twenty lines apart. Nothing
enforced it and nothing named it, so the three that got it wrong read exactly
like the four that got it right.

**A green on a load-sensitive JIT bug is a statement about the host.** The same
binary passed three times and failed six times on one afternoon, with the load
average as the only variable, because tiering decides which of two compiled
bodies serves the call. Any arm of that A/B is worthless without the
`jit-disasm` check that the failing body was compiled at all.
