# The helpful NPE message is lost once a method is JIT-compiled

**Status: OPEN.** 2026-09-10, found by lane 2 of the `--jdk-only` shadow
campaign. Not a `--jdk-only` defect and not a `BigInteger` one: it applies to
every hot method in every program, in both modes. It is filed from that lane
because retiring a shadow is what made it visible.

---

## 1. The measurement

`apps/probes/L2JitNpeProbe.java` asks five null-dereference shapes twice: once
cold, then again after 200 000 warm-up calls with a live receiver. Each shape is
its own static method, so the compiler treats them independently, and no JDK
class is involved anywhere. Binary `06307f0eca53c714`, JDK 25.0.4+7, Linux.

```text
                      HotSpot                              CratonVM
readField cold        ...because "<parameter1>" is null     ...because "<local0>" is null
readField hot         ...because "<parameter1>" is null     null
readArrayLen cold     ...because "<parameter1>" is null     ...because "<local0>" is null
readArrayLen hot      ...because "<parameter1>" is null     null
invokeOn cold         Cannot invoke "...Holder.get()"...    Cannot invoke "...Holder.get()"...
invokeOn hot          Cannot invoke "...Holder.get()"...    null
invokeOnString cold   Cannot read field "s"...              Cannot read field "s"...
invokeOnString hot    Cannot read field "s"...              null
writeField cold       Cannot assign field "value"...        Cannot assign field "value"...
writeField hot        Cannot assign field "value"...        null
```

**Cold, this VM produces JEP 358's message. Hot, it produces none at all** —
`getMessage()` is null, so a caller sees a bare `java.lang.NullPointerException`.
HotSpot's message is identical cold and hot, because C2's implicit null check
deoptimises and lets the interpreter rebuild the message.

The pairing is what makes this readable: the two rows of a pair differ in
nothing but whether the method has been compiled, so the compiler is the only
candidate.

## 2. Why the existing test does not catch it

`vm/tests/jit_npe_message_from_compiled_code.rs` covers this ground and passes,
and its own header explains why it cannot see this:

> **The array shapes never reach those drains.** `jit_npe_with_action` records
> the action and then sets a DEOPT: compiled code does not raise the NPE, it
> bails to the interpreter, which replays the trapping bytecode and raises it
> with the interpreter's own — FULLER — message.

That is the mechanism that works, and it works for the shapes that route through
`jit_npe_with_action`. The shapes in §1 do not: a `getfield` / `putfield` /
`invokevirtual` on a null receiver is caught by the **implicit** null check
(`jit/src/implicit_null.rs`), which arrives as a SIGSEGV and constructs
`NullPointerException { message: None }`. There is nothing wrong with the
existing test — it asserts what it says it asserts. The gap is that the deopt
path and the signal path are two different answers to the same question, and
only one of them carries the message.

Note also the smaller, separate difference visible in the cold rows: for a
method PARAMETER, HotSpot says `<parameter1>` where this VM says `<local0>`.
That one is present cold and hot and is not part of this defect.

## 3. What it costs

It is a diagnostic-quality defect, not a correctness one: the exception type,
the site and the control flow are all right, and only the message is missing.
The cost is that it is missing exactly when someone is reading it — a null
dereference in a hot loop is the case a developer is most likely to be
debugging, and the least likely to reproduce cold.

It also **blocks six rows of lane 2's `BigInteger` retirement**. Those six
(`remainder`, `mod`, `gcd`, `and`, `or`, `xor`) reach real bytecode correctly and
are HotSpot-exact under `--nojit`; with the JIT on they lose the message, which
takes `apps/probes/BigIntegerSweep.java` from 0 differing rows to 6. Retiring
them today would trade a correct message for none, so they are held back in
`RETIRED_SHADOW_L2_TRIPLES`' doc comment with this page named. Any shadow
retirement that moves an object-argument method from a native to real bytecode
will meet the same wall.

## 4. What a fix would have to do

Route the implicit (signal-originated) null check through the same
deopt-and-replay the explicit array checks already use, so the interpreter
rebuilds the message from the trapping bytecode. The machinery exists —
`set_jit_pending_npe_action`, `DrainedJitSignals`, and
`helpful_npe::jit_action_message` are all present, and the four pending-NPE
drains in `jit_bridge.rs` currently build `message: None` unconditionally. Per
that test's header those drains have no production caller today, so wiring them
is a change with a measurable before and after rather than a speculative one.

`apps/probes/L2JitNpeProbe.java` is the acceptance test: five pairs, and a fix
is done when the hot row of every pair equals its cold row.

## 5. Provenance

Found while adjudicating `java/math/BigInteger`'s 24 shadow rows. With all 24
retired, `BigIntegerSweep` went from 0 differing lines to 18; `--nojit` on the
same binary and the same probe reduced that to 6, which is what separated this
defect from the unrelated survivor defect in the other three rows. The
`--nojit` arm is the whole diagnosis, and it cost one run.
