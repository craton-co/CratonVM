# The helpful NPE message was lost once a method was JIT-compiled — FIXED 2026-09-11

**Status: FIXED.** Found 2026-09-10 by lane 2 of the `--jdk-only` shadow
campaign; closed 2026-09-11. Not a `--jdk-only` defect and not a `BigInteger`
one: it applied to every hot method in every program, in both modes.

Acceptance test, the page's own: `probes/L2JitNpeProbe.java`, driven by
`vm/tests/jit_npe_message_hot_equals_cold.rs` and ratcheted into
`tools/e2e-ratchet.txt`. **Every pair's hot row now equals its cold row** — the
page's five shapes, plus three array shapes and one caught-in-a-handler shape
that closing it turned up.

---

## 1. What it was, and the half the page did not record

The filed measurement: cold, this VM produced JEP 358's message; hot — the same
call, after 200 000 warm-up calls — it produced none at all, so a caller saw a
bare `java.lang.NullPointerException`.

Closing it turned up a second, quieter shape the page had not asked about. The
ARRAY forms did not lose their message; they lost HALF of it, which is worse
because the remainder is plausible:

```text
                   HotSpot                            CratonVM, before
lenOf  hot         Cannot read the array length       Cannot read the array length
                   because "a" is null                                     (no clause)
loadOf hot         Cannot load from int array         Cannot load from int array
                   because "a" is null                                     (no clause)
```

A reader has no way to tell a HotSpot-verbatim action half with the `because`
clause missing from one that never had a clause to give. That is why the fix
below does not repair the action table: it stops using it as the source.

## 2. Why the message was missing, in one sentence per door

An NPE raised inside compiled code is not thrown there. The compiled body
signals a flag and returns the `i64::MIN` sentinel, and the throwable is built
later, somewhere that no longer knows what trapped. There are two such places
and they had the same defect:

* **The interpreter's post-JIT drains** (four, in `jit_bridge.rs`) built the
  message from the *action code* alone — a fixed string per opcode family, with
  no `because "…" is null` clause, and no string at all for the shapes whose
  helper records no action: `getfield`, `putfield` and `invoke*` on a null
  receiver, which is every object-receiver dereference there is.
* **`jit::helpers::materialize_implicit_signal`**, which serves a null deref
  CAUGHT by the compiled method's own handler, passed `None` unconditionally.

## 3. The fix

**The trapping BYTECODE is the source, not a summary of it.**
`vm/src/runtime/interpreter/jit_npe_message.rs` takes the trapping `(method,
bci)`, reads `code[bci]`, and answers the same question the interpreter answers
at the same opcode, through the same `helpful_npe` analysis. A hot row equals
its cold row by construction rather than by a table kept in sync with the
emitter by hand. All five doors — the four drains and the caught-handler
constructor — go through it, so `getMessage()` cannot depend on which one
serviced the trap.

**The trapping `(method, bci)` comes from the snapshot that already exists.**
`jit::helpers::snapshot_trap_frames` captures the live compiled frames at the
trap so the NPE can have a stack trace; it names the trapping program point
exactly, so it answers this question too. Three things had to be added for it to
name every shape:

| shape | what was missing | what was added |
|---|---|---|
| `getfield` | the helper has the receiver (null) and the slot index — neither names the field or the bci | a 24-bit NPE trap-SITE key in bits 32..56 of `jit_getfield`'s existing third argument (`cratonvm_jit_api::GETFIELD_NPE_SITE_SHIFT`), recorded by `record_npe_trap_site` at every emitter arm |
| `getfield`, optimizing tier | IR artifacts published **no `npe_trap_map` at all** — `CompiledMethod::npe_trap_map` was `Default` for every one of them, so a key would have missed | `ir_lower::record_npe_trap_site`, the single-pass recorder's twin, built from `IrInlineFrameSites::enclosing_bci_at` / `chain_at`, published beside `inline_frame_map` |
| `invoke*` | the null is detected in the dispatch helper, which has no per-site key | `npe_action::INVOKE_RECEIVER` — an action code that names the OPCODE FAMILY rather than a message, recorded by the dispatch helper, the MIC helper and all seven direct-bound intrinsics |

**A bci is used only when something corroborates it.**
`jit_npe_message::site_is_trustworthy` demands one of two warrants: the site was
described by the emitter (a trap key, or an exact-return-address inline chain),
or the recorded action code agrees with the opcode found there. Without one, the
caller keeps the old action-only message. That is not a formality — a bci
recovered from the frame's safepoint-id slot can be one safepoint behind, and
`line_number_for_bci_in_method` and the operand-stack simulation both answer
confidently for the wrong index. A fluent sentence about the wrong dereference
is worse than none, and a reader cannot tell them apart.

## 4. Found while closing this, and fixed with it

Both are in the compiled-frame snapshot rather than in the message, and both
were found by building fixture variants to test the diagnosis above.

* **A discarded throwable took the snapshot with it.** The two doors that route
  an implicit signal into a compiled callee's own exception table materialise
  the throwable, and if the table does not match they discard it and restash
  only the FLAG. The snapshot went with the discarded throwable, so the
  caller-side drain rebuilt the exception from nothing: `len=1 [main]` where
  HotSpot reads five. Both doors now put it back
  (`restash_jit_pending_trap_frames`), and `set_jit_pending_npe_flag_only` no
  longer zeroes the JEP-358 action code it never consumed.
* **A snapshot spliced onto a trace that already held it printed frames twice.**
  `append_snapshotted_compiled_frames`'s premise is that the compiled frames
  have unwound by construction time — `materialize_implicit_signal` breaks it,
  because it builds the throwable inside the helper with every frame still live,
  so `fillInStackTrace` has already walked them. `trailing_overlap` now appends
  only the part the capture did not already see. Zero overlap — every shape
  where the frames HAD unwound — is byte-for-byte the previous behaviour. It is
  the maximal prefix-of-`fresh` / suffix-of-`trace` match and not a membership
  test, because genuine recursion appears twice in BOTH lists and a membership
  test would delete a real frame.

## 5. What is left, and why it is left

* **The `getfield` arm with no field metadata.** One emitter arm passes `0` for
  the whole argument because it has no resolved field at all (see the comment at
  `bytecode_walk.rs`'s fourth `getfield` arm), so it can carry no key either. It
  is the same arm that already cannot set `GETFIELD_EXPECT_REFERENCE`; the place
  to close it is the resolution that failed.
* **An `invoke*` inside a spliced body, on a coarse chain.** The trap is
  corroborated by `INVOKE_RECEIVER` against the opcode at the recovered bci,
  which is the compiling method's own program point; a splice described only by
  the coarse `safepoint_bci` key is used, because it is what the trace already
  PRINTS for that frame, but it is not proof. A key at the dispatch site would
  make it one.
* **Lane 2's six held `BigInteger` rows.** This page was the named blocker on
  `remainder`, `mod`, `gcd`, `and`, `or`, `xor` in
  `docs/known-issues/jdk-only-lanes/lane-2-lang-values.md` and in
  `RETIRED_SHADOW_L2_TRIPLES`' doc comment. The blocker is gone; the retirement
  is that lane's to make and to measure, and the rule it holds them under is
  structural rather than a list of six, so it is not lifted here.

## 6. The measurement that closes it

Windows, release binary, Temurin 25.0.3+9, `probes/L2JitNpeProbe.java`, default
configuration:

```text
readField      cold/hot  Cannot read field "value" because "h" is null
readArrayLen   cold/hot  Cannot read field "arr" because "h" is null
invokeOn       cold/hot  Cannot invoke "L2JitNpeProbe$Holder.get()" because "h" is null
invokeOnString cold/hot  Cannot read field "s" because "h" is null
writeField     cold/hot  Cannot assign field "value" because "h" is null
caughtHere     cold/hot  Cannot read field "value" because "h" is null
lenOf          cold/hot  Cannot read the array length because "a" is null
loadOf         cold/hot  Cannot load from int array because "a" is null
storeTo        cold/hot  Cannot store to int array because "a" is null
```

Eighteen rows, nine pairs, every pair equal — and
`java -XX:-OmitStackTraceInFastThrow` prints the same eighteen. What each hot
row was before:

* the five §1 shapes and `caughtHere`: `java.lang.NullPointerException: null`;
* the three array shapes: the action half alone, with the `because "a" is null`
  clause silently missing.

The three array pairs are in the probe because of the second line: they are the
rows that regress if `jit_npe_message` ever declines a site, and the way they
regress is to a message that still reads like a HotSpot one.

## 7. Reproducing

```
JAVA_HOME=<jdk25> PATH=<jdk25>/bin:$PATH \
CRATONVM_BIN=target/release/cratonvm CRATONVM_REQUIRE_E2E=1 \
cargo test -p cratonvm-vm --test jit_npe_message_hot_equals_cold
```

`CRATONVM_DBG_JITNPE=1` prints, per JIT-originated NPE, the trap site the
rebuild recovered and the message it built — or, when it declined, the site it
refused and the action code it fell back to. A refusal is otherwise invisible in
the product, which is the blind spot that let the whole JIT-NPE message
apparatus sit wired to nothing until 2026-09-06.
