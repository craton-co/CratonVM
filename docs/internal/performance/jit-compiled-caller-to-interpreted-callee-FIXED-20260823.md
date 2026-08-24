# A compiled caller calling an INTERPRETED callee — FIXED 2026-08-23

| | |
|---|---|
| **Status** | **FIXED.** All four dispatch kinds now enter an uncompiled callee through the call site's own cached interpreter frame template. Compiling the caller is no longer a pessimisation on any of them |
| **Opened** | 2026-08-22; `invokestatic` fixed the same day, `invokevirtual`/`invokeinterface`/`invokespecial` on 2026-08-23 |
| **Closed by** | `perf/filechannel-vector-webclient-residuals-20260823` |
| **Measured effect** | `invokestatic` 1310 -> 259 ns/op (2026-08-22). `invokevirtual`/`invokeinterface`, `probes/XferVirtProbe.java`, one binary: **2 044 -> 440 ns/op** on Azure, **3 049-5 635 -> 325-424 ns/op** on Windows — both now BELOW the both-interpreted control |

## The defect

A compiled caller calling a callee the JIT did not compile fell out of
`jit_invoke_dispatch`'s fast arms into `crate::vm::invoke_or_native` — the
fully name-keyed generic dispatch — and re-derived a constant on every call: it
re-resolved the class by name, re-ran the native-override arbitration,
re-cloned the `CodeAttribute` and re-padded the bytecode. The interpreter pays
none of that; its call sites hold a `CachedInvokeTarget::Bytecode` with a
prebuilt `Arc<CachedBytecodeMethod>`.

The compiled side had a cache for a COMPILED callee (`DISPATCH_CACHE`,
`VIRTUAL_DISPATCH_CACHE`) and **none at all for an interpreted one**, which is
why compiling the caller and not the callee measured 5x slower than compiling
neither.

## What it took, per kind

| kind | memo | key | what makes it safe |
|---|---|---|---|
| `invokestatic` (3) | `try_jit_static_bytecode_callee` | site | `jit_static_owner_override` declines, i.e. no loader-faithful owner differs from the global by-name answer |
| `invokevirtual` / `invokeinterface` (0, 2) | `try_jit_virtual_bytecode_callee` | (site, **receiver class**) | resolution starts from the receiver's `ClassId`, never a name — so the by-name loader hazard cannot arise, and a site that goes polymorphic simply misses |
| `invokespecial` (1) | `try_jit_special_bytecode_callee` | (site, **resolved owner class**) | the kind-1 arm has ALREADY resolved the owner through the CALLER's loader; that `ClassId` is the key, and when the resolution is unavailable the memo is not consulted at all |

All three share one refusal gate, and reusing it rather than re-spelling it is
deliberate — a fourth spelling is how they would come to admit shapes the
others refuse. It declines: a name `invoke_or_native` special-cases
(`site_name_is_special_cased`, which includes `<init>`/`<clinit>`); ANY native
registered anywhere with the `(name, descriptor)`, which removes the
native-override, `SyntheticStub`-yield and redefine-shadow questions rather
than reproducing them; anything `build_lambda_impl_cached` will not build a
template for; an arity disagreement; and an uninitialised declaring class.

The virtual memo additionally refuses an array receiver and a `ClassId(0)`
receiver — an array header carries its COMPONENT class id, so `(site, class
id)` does not identify one.

## The numbers

`probes/XferVirtProbe.java` is new: a hot loop calling `step(int)` on an
`invokeinterface` site with one concrete receiver — the monomorphic shape a
real call graph is mostly made of. `probes/XferProbe.java` cannot answer this
question, because the static memo already serves its site.

One binary, `CRATONVM_JIT_DENY=XferVirtProbe$Impl.step` to keep the callee
interpreted while the caller stays compiled. Azure Linux host 2:

| arm | r1 | r2 | r3 |
|---|---:|---:|---:|
| compiled caller -> interpreted callee, memo **off** | 2 096 | 2 044 | 1 967 |
| compiled caller -> interpreted callee, memo **on** | **434** | **449** | **440** |
| both interpreted (`--nojit`) — the control | 727 | 632 | 655 |
| both compiled | 34 | 33 | 36 |

`sink` is byte-identical across all four arms. The third row is what makes the
second readable: the transition used to cost **3x more than not compiling the
caller at all**, and now costs less.

Windows 11, same binary, same switch, noisier host: memo off 3 049 / 5 635,
memo on 325 / 351 / 411 / 424, `--nojit` control 472-566.

`out_virt_bc` / `out_virt_bc_refused` and `out_special_bc` /
`out_special_bc_refused` join the `CRATONVM_DBG=mic-prof` `[DISP_CENSUS]` line
beside the static pair, and each kind has its own switch —
`CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0`,
`CRATONVM_JIT_SPECIAL_BYTECODE_CALLEE=0`,
`CRATONVM_JIT_STATIC_BYTECODE_CALLEE=0` — so any one of them is a same-binary
A/B.

## What this did NOT buy, and the count that says so

The page opened as the explanation for
`webclient-integration-tests-reactive-exchange-gap`'s "the JIT is a wash"
finding, and predicted that fixing the transition would move that class. **It
does not, and the census now says why with counts rather than with a wall-clock
A/B on a shared host.**

`CRATONVM_DBG=mic-prof` on 300 `ExchangeProbe` exchanges (Azure, the class's
own probe):

```
disp_calls=10232   mic_calls=4270   hit_entry=0   hit_noentry=3280
out_virt_bc=44     out_virt_bc_refused=3298
out_special_bc=0   out_special_bc_refused=0
```

Ten thousand `jit_invoke_dispatch` calls for three hundred exchanges, and
`hit_entry=0` — not one MIC dispatch found a compiled callee. The
compiled→interpreted transition is reached about **11 times per exchange**. At
the ~1 600 ns this change saves per transition that is **~18 µs against
~30 000 µs, or 0.06%**.

So the isolated 4.6x is real and the workload-level silence is also real, and
they do not contradict each other: **that workload barely enters compiled code
at all.** Do not quote the isolated number as a suite number, and do not
re-open this page because a reactive workload did not move — read
`disp_calls` first.

## §3 — the `invokedynamic` corollary is unchanged

A method containing an unbridged `invokedynamic` is denied OSR outright and
retired with `MakeNotCompilable` after its first compiled execution, so every
lambda-creating method becomes an interpreted callee. That is still true, and
this fix does not touch it — `probes/IndyScopeProbe.java` measured
`osrShape` 1 097 vs 1 170 ns/op and `invokeShape` 1 644 vs 1 725 with the memo
on and off, a wash inside noise. The reason is the same one the census gives:
when the method containing the indy is interpreted, so is its CALLER, so there
is no compiled→interpreted transition to serve. That half belongs to the OSR /
indy-bridging work, not here.

## Repro

```bash
CRATONVM_JIT_DENY='XferVirtProbe$Impl.step'   # keep the callee interpreted
CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0        # the memo off, same binary
CRATONVM_JIT_SPECIAL_BYTECODE_CALLEE=0
CRATONVM_DBG=mic-prof                         # [DISP_CENSUS] + [MIC_PROF]
probes/XferVirtProbe.java   # 2000000
probes/XferProbe.java       # the invokestatic counterpart
probes/IndyScopeProbe.java  # 300000 2
```

All are pure CPU with no sockets, so unlike the WebClient exchange probe they
are readable on a loaded shared host.

Regression suite on the fixed binary: **70 passed, 0 failed**;
`cargo test -p cratonvm-vm --lib jit::helpers` 87 passed, including
`a_jit_generation_change_clears_every_site_keyed_memo` with the new memo's
population line.
