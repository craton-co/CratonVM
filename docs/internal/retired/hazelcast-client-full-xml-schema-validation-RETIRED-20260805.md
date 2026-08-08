# `HazelcastAutoConfigurationClientTests` — RETIRED 2026-08-05

**The optimizing IR backend never wrote the return register on a `void`
return, so any void method it compiled could hand its caller the VM-wide
"the callee trapped" sentinel by accident.**

Fixed in `jit/src/ir_lower.rs` (`Op::Return`): emit `XOR EAX, EAX` when the
`Return` node carries no value, exactly as the single-pass backend's `0xb1`
arm has always done. One instruction, pinned by
`ir_lower::tests::a_void_return_writes_the_return_register`.

Superseded `docs/known-issues/springboot/hazelcast-client-full-xml-schema-validation-20260805.md`,
whose diagnosis (a schema-version mismatch, caused by either a classpath
resource-resolution bug or a stale `~/.m2` artifact) was wrong in every part.

## Result

| arm | before | after |
|---|---|---|
| `HazelcastAutoConfigurationClientTests` (Azure Linux, jit) | FAIL `tests=0 containersFailed=1` in 2.6s | **PASS 12/12, 3/3 runs**, ~61s |
| `probes/HzConfigLoadProbe` ×5 | `ok=0 fail=5` | **`ok=5 fail=0`** |
| spurious callee-deopt servicings per `Config.load()` | 11 | **0** |

## The defect

`i64::MIN` in the return register is this VM's "the callee trapped" sentinel.
A `void` method has no return value, so unless its exit writes one, RAX carries
whatever the method's last operation left there — and can equal the sentinel
by accident.

Every consumer reads that register raw and cannot tell the two apart:

* the single-pass inline MIC/PIC cascade's `emit_inline_callee_deopt_check`,
* the megamorphic hashed stub's `emit_callee_deopt_check`,
* `jit_invoke_virtual_mic`'s `rc == i64::MIN`,
* the interpreter's post-JIT drain.

A false positive there is not a wasted branch.
`handle_compiled_callee_deopt_sentinel` resumes a stashed callee frame
(`try_resume_trapped_callee`) and drains the thread's **entire** pending-signal
record (`take_all_jit_signals`) — on behalf of a call that neither threw nor
deopted.

The single-pass backend has always zeroed RAX on `0xb1`, and its comment names
this exact hazard. `ir_lower`'s `Op::Return` only ever wrote RAX when the node
carried a value. So the bug was latent in **every void method the optimizing
tier compiled**, VM-wide — Hazelcast's config validation is simply where it was
finally cornered.

Measured: `CRATONVM_DBG_CALLEE_DEOPT=1` over one `Config.load()` reported 11
servicings and **every one was a void callee** —
`QName.setValues(QName)` ×7, `XMLAttributesImpl.addAttributeNS`,
`ValidatorHandlerImpl.fillXMLAttribute` and `.fillXMLAttributes2`. None of them
can throw.

## Why the helper path was green and the inline path red

Both reach the same compiled entry with the same ABI, so the asymmetry was the
whole puzzle. `jit_invoke_virtual_mic`'s own `rc == i64::MIN` branch hands the
sentinel back to the compiled caller, whose `emit_post_invoke_exception_check`
deopts it to the interpreter — which re-executes the call correctly, just
slowly. The inline cascade instead calls `jit_service_callee_deopt` **at the
raw call site** and carries on. Same false signal, different recovery.

That is why every lever that routed this one site back through the helper
(`CRATONVM_JIT_SP_INLINE_IC=0`, `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`,
`CRATONVM_JIT_SP_IC_DENY=<site>`, `CRATONVM_JIT_DENY=xni/QName.setValues`)
turned it green while leaving the actual defect untouched.

## How it was cornered

The bisect ran on `probes/HzConfigLoadProbe` — `Config.load()` in a loop, no
Spring, no JUnit, ~1 second, deterministic 5/5 on Azure Linux — instead of the
80-second test class.

New levers, all inert unless set, all kept:

| lever | what it separates |
|---|---|
| `CRATONVM_JIT_SP_IC_ONLY` / `_DENY` | one inline-cache SITE, matched against `<caller label>\|\|<callee class>.<callee method>` |
| `CRATONVM_DBG_SP_IC_SITES` | lists every site the cascade is emitted at, so the bisect has a candidate set instead of guessed names |
| `CRATONVM_JIT_SP_INLINE_PIC` / `_MIC` / `_MEGA` | the three cascade shapes |
| `CRATONVM_JIT_SP_IC_DEOPT_CHECK` = `0` \| `void` | the sentinel comparison, everywhere or only for void callees |
| `CRATONVM_DBG_IC_PUBLISH` | what the helper publishes into a site's MIC/PIC, beside what the entry's own `CompiledMethod` says |
| `CRATONVM_DBG_CALLEE_DEOPT` | every servicing of a callee sentinel, with the callee's triple and return type — **this is the one that named the bug** |
| `CRATONVM_JIT_MIC_PUBLISH_IR_CALLEES` | keeps IR-backend bodies off the MIC/PIC, as `callee_barred_by_table` does for handler-bearing ones |

`CRATONVM_DBG_JIT_CODE` also now dumps optimizing-tier bodies (tagged
`backend=ir`). It previously dumped only single-pass finalizations, so asking
it for `QName.setValues` handed back a **different artifact for the same
method** — every conclusion drawn from that disassembly was about code the
cascade never calls.

Site chain, each step measured: whole cascade → `xerces/` → `xerces/internal/util/`
→ caller `XMLAttributesImpl` → caller `addAttributeNS` → `@pc=131`,
`invokevirtual QName.setValues(QName)V`. `SP_IC_ONLY` on that one site alone
reproduces; `SP_IC_DENY` on it alone clears.

## Refuted, each by measurement rather than argument

* **`@ClassPathOverrides` / a stale `~/.m2`.** The test class carries no such
  annotation and has no annotated superclass, so it resolves against the Gradle
  test classpath. The standing Maven-cache guidance does not apply to it.
* **Resource shadowing.** Exactly one `hazelcast` jar (5.5.0) on the module's
  test classpath. There is no second `hazelcast-config-*.xsd`.
* **The document the page named.** The error is in the *server* `schema/config`
  namespace; the test's own fixtures are 16–18 line *client* configs. The
  failing document is Hazelcast's bundled `hazelcast-default.xml`, loaded by
  `Config.load()`.
* **A parse or DOM fault.** `probes/HzWarmShapeProbe` and
  `probes/HzXsdShapeProbe` hash the instance DOM and the XSD's own parse —
  elements, depth, namespace URIs and every attribute's uri/localname/value —
  **in the warm process, while `Config.load()` is failing in it**: 162/5 and
  2692/8, byte-identical to HotSpot, zero diff lines.
* **A miscompile of `addAttributeNS` or `QName.setValues`.**
  `probes/AddAttrNsProbe` and `probes/QNameSetValuesProbe` drive the real JDK
  classes (not replicas) for 20–40k rounds and match HotSpot exactly.
* **GC.** `--verbose:gc` reports `minor=0 major=0` for the whole reproducer;
  `--Xmx 256m` / default / `8g` all failed identically.
* **A wrong published entry or ABI flag.** `CRATONVM_DBG_IC_PUBLISH` shows the
  published entry, the entry's owner, and both `needs_context` values agreeing,
  5/5.
* **The megamorphic hashed stub.** `CRATONVM_JIT_SP_INLINE_MEGA=0` still red —
  and PIC-only and MIC-only are each red, so both cascade shapes reproduce
  independently and the fault is in what they share.
* **The recycled-`JitInvokeInfo` family** (`383e7f5cf`, `9d0636e73`) and its
  consumers: `NO_NATIVE_SITE_CACHE=1`, `METHOD_SITE_CACHE=0`,
  `FIELD_SITE_CACHE=0`, `LEAK_CODE=1`, `FREE_CODE=0`, `POISON_FREE=1` — all
  still red on a binary carrying both fixes.
* **Every IR pipeline lever** — `ISEL_EMIT`, `LINEAR_SCAN`, `RELOC_EMIT`,
  `IR_CALL{,_VIRTUAL,_SPECIAL}`, `IR_LONG`, `IR_FP`, `FORCE_C2` — all inert.

## Two traps recorded

**`CRATONVM_JIT_NO_DUP_X1=1` turning it green means nothing.** That arm calls
`self.fail(...)` when the flag is set, so it merely stops the method compiling —
identical to `CRATONVM_JIT_DENY` on it. Both `dup_x1` implementations were read
against JVMS and are correct, and `CRATONVM_DBG_DUPX_TRACE=1` confirms the
rotate is right in this very method. `CRATONVM_JIT_DUPX_EAGER_CANON=1` also
fails, which by that flag's own documentation would point at the rotate model —
another reason the reading looked plausible. It is still wrong.

**A cold shape probe proves nothing here.** `XmlDomShapeProbe` showed CratonVM's
DOM matching HotSpot's exactly and was cited as evidence the parse was sound —
but it ran in a process that had done nothing else, so the methods the failure
needs compiled were still interpreted when it measured. Re-running the same
measurement *after* `Config.load()`, in the failing process, is what made it
evidence.

## Verification

* `HazelcastAutoConfigurationClientTests` — PASS 12/12, three consecutive runs
  on Azure Linux.
* `cargo test -p cratonvm-jit` — green, all targets (1956 lib tests).
* `cargo test -p cratonvm-vm` — green except two **pre-existing** failures in
  `class_loader_unload_regression`
  (`custom_loader_metadata_is_reclaimed_{with,without}_jit`). Confirmed
  pre-existing by re-running that target with the one emit line reverted: both
  fail identically. `…_without_jit` cannot be reached by a codegen change at
  all, which is the first tell.
* Regression test verified RED with the emit line removed (`void=0, value=0`)
  and GREEN with it restored.
* Spring Boot A/B, pre-fix vs post-fix binaries **interleaved in both orders**,
  13 classes from 13 different modules, 4 runs each — no change. Four cells
  came back verdict-less during a load-100 window and were re-run at load ~15;
  the blanks fell on both arms, so they were host noise, not a signal.

### The residual sweep, and what it does NOT show

All 42 classes the 2026-08-05 full suite scored FAIL / CRASH / BOTH-FAIL were
re-run on a binary carrying this fix, and 39 of them passed. **That number is
not this commit's.** The 08-05 table was produced by a binary predating several
intervening fixes — the recycled-`JitInvokeInfo` family (`383e7f5cf`,
`9d0636e73`) and the annotation-`Class`-element cluster among them — so a flip
against it attributes to "everything since 08-05".

Re-run against the real control (`/tmp/cvm-hzic4`: every one of those fixes,
lacking only the void-return write):

| | pre-fix | fixed |
|---|---|---|
| PASS | 39 | 41 |
| FAIL | 2 | 1 |
| no verdict | 1 | 0 |

**The only class that differs between the two arms is
`HazelcastAutoConfigurationClientTests`** — `tests=0` on pre-fix, 12/12 on
fixed.

The three cells that needed a second pass, all re-run paired at load ~6:

* `RabbitAutoConfigurationTests` — PASS 78/78 on both. Its first verdict-less
  POST result was the load-90 window, not a regression.
* `ConfigurationPropertySourcesTests` — PASS 11/11 on the fixed binary; the
  pre-fix binary produced no verdict on either attempt, so this one is
  unscoreable rather than an improvement. Counted as neither.
* `NettyReactiveWebServerFactoryTests` — FAIL on both, and its failure MODE
  varies between runs (`tests=36 failed=1` once, `tests=0 containersFailed=1`
  the next). Red on both arms either way: a separate residual.

So this change closes the class the page named. The defect it removes is
VM-wide and latent in every void method the optimizing tier compiles, but this
42-class sample does not demonstrate a second victim — do not cite it as if it
did.

