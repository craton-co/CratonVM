# `invokespecial` on a null receiver lost its NPE again — FIXED 2026-09-08, and §2 of this page named the wrong inliner

> **RETIRED 2026-09-08. The defect is fixed; §2's mechanism was WRONG.** Everything
> below the banner is kept unedited so the error stays legible -- including its
> now-false `Status: OPEN` line and its "No fix in this page" claim. Read the
> banner as the status; read the body as what was believed on the day.
>
> Fixed by `936536100` (`jit,vm: three dispatch checks that existed on one
> lowering path and not on its sibling`, defect 2). The write-up is
> `warm-invokespecial-on-a-null-receiver-runs-the-callee-again-FIXED-20260908.md`
> in this directory.
>
> **What §2 got wrong.** It named the single-pass backend's `try_emit_inline`
> in `jit/src/x64/bytecode_walk.rs` as the optimisation that claimed the site.
> The real culprit was the OPTIMIZING (C2/IR) tier: `IrBuilder::begin_splice`
> bound arg 0 into callee local 0 and walked in, and since splicing deletes the
> `invokespecial` that JVMS §6.5 hangs the NPE on, a body that never touches
> `this` left no fault to fall back on. The fix adds `receiver_is_arg0` to
> `IrInlineSite` and an `Op::Guard` on `Cmp(Ne, receiver, aconst_null)` ahead
> of the splice, so taking it re-executes the invoke in the interpreter, where
> the canonical NPE is raised by the code that owns it.
>
> **Why the evidence in §1 could not tell them apart, which is the reusable
> part.** `NrpVariants`' callees are all small, and a small private callee is
> claimed by BOTH inliners — the single-pass one and the IR one. Either fix
> makes the site green, so a green small callee says nothing about which tier
> was at fault, and `CRATONVM_DBG_JITC` naming an inline plan does not say
> which inliner will finally own the site in a warmed method. The fix's test
> settles it with a second callee, `privateCallBig`, sized past the single-pass
> budget (`Refuse(CalleeTooLarge)`) and inside the IR tier's, which is four
> times larger. Both arms reported NO-THROW; only the big one rules the
> single-pass inliner out. **A discriminating probe needs a body each candidate
> door sizes differently.**
>
> §3 (two gates, and CI ran the blind one) and §4 (the RED targets were a
> number in a header) stand as written. `null_receiver_cached_invoke` was
> ratcheted on 2026-09-08 after this fix, which was §5's step 3.


**Status: OPEN — MEASURED 2026-09-08.** Windows 11, JDK 25 (Temurin
`25.0.3+9`, the same image as the oracle), release binary built from
`origin/dev`. **Mode-independent**: `--real-jdk` and `--jdk-only` both. No fix
in this page; it is the diagnosis, the reproduction, and the reason CI is blind
to it.

**Found by** running the 107 `apps/probes/` files that no gate schedules through
the three-arm strict-corpus harness — the first time any of them had been run
that way on Windows.

---

## 1. What happens

JVMS §6.5: *"if objectref is null, the invokespecial instruction throws a
NullPointerException."* Once the caller is JIT-warm, it does not.

`apps/probes/NrpVariants.java` is the three-body probe, and it reproduces the
table of a defect that was recorded **FIXED on 2026-08-30** — row for row:

```text
  private callee body     HotSpot   --nojit   jit (recorded "before")   jit (TODAY)
  return 3;               NPE       NPE       NO-THROW(3)               NO-THROW(3)
  return this.x;          NPE       NPE       NPE                       NPE
  return helper();        NPE       NPE       NO-THROW(5)               NO-THROW(5)
```

The callee runs with `this == null`. `return this.x;` is right only
incidentally: its body dereferences `this`, so the spliced code faults on its
own.

**It is not strict-only, and my own sweep first said it was.** The sweep
classified `NrpVariants` as `STRICT-ONLY` because in that run the `--real-jdk`
arm had not warmed the site by the time the check ran. Re-run directly, both
shipping modes fail:

```text
--jdk-only  (jit)      NO-THROW(3)  NPE  NO-THROW(5)
--real-jdk  (jit)      NO-THROW(3)  NPE  NO-THROW(5)
--jdk-only --nojit     NPE          NPE  NPE
```

A one-run mode label on a JIT-timing-dependent row is a coin flip; the arm
that "passes" is the arm that did not compile in time.

## 2. The mechanism — an optimisation claims the site before the check

**The check is still in the tree, and it is not the one that runs.**
`jit/src/x64/bytecode_walk.rs` carries it on the direct-call arm, with the
comment and the measured table the 2026-08-30 fix left behind:

```rust
// The receiver is argument 0 of every invoke that reaches this arm …
if let Some(receiver) = arg_slots.first() {
    self.load_slot_to_reg(RAX, *receiver);
    self.emit_precise_null_check_field_store();
}
```

That arm is never reached for these callees, because `try_emit_inline` is
consulted first (`bytecode_walk.rs:9240`, "*Check for inline site
(invokespecial only — virtual/interface not eligible)*") and it **splices the
body instead of emitting a call**. `CRATONVM_DBG_JITC` says so directly:

```text
[cratonvm-jitc] inline-plan pc=1 NrpVariants$Impl.constBody()I: DirectBind (cost=Some(2) budget_left=750)
[cratonvm-jitc] inline-planned NrpVariants$Impl.constBody()I @pc=1
[ir] inline-plan NrpVariants$Impl.viaCall(…)I: 1 site(s), 2 spliced bodies, 7 bytes appended
[cratonvm-jitc] inline-plan pc=1 NrpVariants$Impl.fieldBody()I: DirectBind (cost=Some(15) budget_left=750)
```

`jit/src/x64/inlining.rs` emits a receiver null check in exactly one place —
for a `putfield` **inside** a spliced body — and none for the spliced call's own
receiver. So the splice drops the check the direct-call arm would have made.

This is the shape the repo already has a name for: *an optimisation that claims
a site first silences the gate behind it.* The 2026-08-30 fix is intact; it
simply stopped being on the path.

## 3. Why CI cannot see it — two gates, and the blind one is the one that looks

There are two null-receiver gates, and they disagree:

| gate | in `tools/e2e-ratchet.txt`? | result with a real binary |
|---|---|---|
| `jit_null_receiver_npe` | **yes** | **ok**, 0.77 s |
| `null_receiver_cached_invoke` | **no** | **FAILED**, 4.15 s |

```text
test warm_null_receiver_invokes_throw_npe_jit ... FAILED
  `warm-invokespecial` on a null receiver did not throw NullPointerException
  once its inline cache was warm (jit=true) … JVMS §6.5 violation
  warm-invokespecial=NO-THROW(3)
```

The failing gate's inline probe uses `private int privateCall() { return 3; }`
— precisely the regressed shape. The ratcheted one covers a shape that still
throws, so it is green and says nothing.

**And without a binary the failing gate PASSES in 0.00 s.** `vm/tests/common`
turns a missing binary/JDK into `eprintln!` + `return`, which cargo prints as
`test … ok`. `CRATONVM_REQUIRE_E2E=1` converts that skip into a panic, and
ci.yml's step that sets it iterates `tools/e2e-ratchet.txt` — which does not
list this target. Reproduce the difference in one pair of commands:

```bash
cargo test -p cratonvm-vm --test null_receiver_cached_invoke
#   ok, 0.00s   <- what CI sees

CRATONVM_BIN=<binary> JAVA_HOME=<jdk> \
  cargo test -p cratonvm-vm --test null_receiver_cached_invoke
#   FAILED, 4.15s
```

## 4. The inventory counts this target and does not name it

`tools/e2e-ratchet.txt`'s header records the 2026-08-30 survey as
**"73 RUNS, 19 VACUOUS, 3 RED out of 95"**. The file then names all 19 VACUOUS
targets and the 1 FLAKY one, each with the reason — under a heading that says
exactly why:

> *Listed here because the alternative is that they stay invisible — which is
> how they got here.*

**The 3 RED were named nowhere.** They were a number in a header. So a target that
RUNS and FAILS — which is what a RED is — is less visible than one that merely
skips, and `null_receiver_cached_invoke` is one of them. That asymmetry is the
reason a JVMS §6.5 regression sat unnoticed: the file's own convention would
have surfaced it, and the convention was applied to two of the three
categories.

## 5. What to do, in order

1. ~~**Name the RED targets in `tools/e2e-ratchet.txt`**~~ — **DONE with this
   record.** Both REDs now have a name and a one-line reason, in the same
   commented form the VACUOUS and FLAKY blocks already use. Comments only: the
   ratcheted list is unchanged, so no job changes colour.
2. **Emit the receiver null check on the inline-splice path** for an
   `invokespecial` site whose spliced body does not provably dereference
   `this`. `emit_precise_null_check_field_store` is already used inside
   `inlining.rs`; the care is in the trap bci / safepoint id, which
   `record_npe_trap_site` and `null_check_store_stubs` exist for. **Not
   attempted here** — it is a hot-path change and it deserves a lane that can
   iterate on it, not the tail of a long session.
3. **Then ratchet `null_receiver_cached_invoke`**, so the next regression of
   this fix is a red build rather than a probe nobody ran.

## 6. What this does NOT claim

* **No fix is proposed as verified.** §5.2 is a direction, not a patch.
* **The survey has since been run to completion**, so §4's gap is now closed
  in the file itself: 2026-09-08, release binary, Temurin 25.0.3+9, Windows —
  **79 RUNS, 14 VACUOUS, 2 RED of 95** (was 73/19/3). The second RED is
  `stack_trace_across_tiers`, whose `after_main_osr` case loses the
  `mid`/`outer` frames to *"the callees the JIT inlined into their caller's
  artifact contributing no frame at all"* — **the same family as this page**:
  inlining dropping a JVMS-visible property that the un-inlined path keeps.
  Both are now named in `tools/e2e-ratchet.txt`; neither is ratcheted, so no
  job changes colour.

> **SUPERSEDED 2026-09-09 — the sentence above ("neither is ratcheted, so no
> job changes colour") is no longer true, and neither is "the second RED".**
> `stack_trace_across_tiers` was fixed by `c798aae82` at 16:37 on 2026-09-08,
> eighteen minutes after the survey line above was written, and is ratcheted
> in `tools/e2e-ratchet.txt` as of 2026-09-09 (10/10 runs, three arms
> byte-identical on all three rows). The cause named above is also wrong for
> that target: it was not the inline frame map dropping frames but the IR tier
> splicing callee bodies and recording no frame rows at all, which is why
> `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` did not revert it. The family claim
> ("inlining dropping a JVMS-visible property") still holds; the mechanism
> does not. Body left unedited.
* **The 2026-08-30 fix is not accused of being wrong.** Its code is present and
  its comment still describes its own emitter correctly. What changed is which
  emitter gets the site.
