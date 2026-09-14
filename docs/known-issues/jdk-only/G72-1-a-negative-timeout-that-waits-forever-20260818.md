# G72-1 — a negative timeout that waits forever

> **RESOLVED 2026-08-18. The sweep now runs to completion: all 32 rows match.**
>
> **The "broken toolchain" in the previous banner was my own mess, and the
> cause is worth more than the fix.** rustc was crashing with
> `STATUS_STACK_BUFFER_OVERRUN` on crates that had compiled minutes earlier,
> including on unmodified `HEAD` — which I read as a host failure. It was not.
> A `cratonvm.exe` left running by **this record's own hang** was holding the
> output binary open and consuming memory, alongside orphaned `rustc`
> processes from builds I had stopped. Killing them made the build succeed in
> 17 seconds. A liveness defect does not only hang the program under test; it
> leaves a process behind that breaks the next thing you do, and the symptom
> arrives disguised as something else entirely.
>
> Two corrections to the body below, both found by dumping the native registry
> BEFORE editing:
>
> * §2 calls `Thread.join(-1)` a correctly-written sibling and reasons from
>   it. **`Thread.join` is not registered as a native at all** — it runs real
>   JDK bytecode and gets the contract for free. One implementation and one
>   absence, not two implementations.
> * The overloads do not share a message: `wait(-5)` is
>   `"timeout value is negative"`, `wait(-5, 0)` is
>   `"timeoutMillis value is negative"`. Ours said
>   `"Object.wait: timeout value is negative"` for the second — wrong prefix
>   AND wrong noun.
>
> **What the other 28 rows turned out to be.** Once the hang was gone the
> sweep ran, and the axis was in far better shape than §4 assumed: every
> exception TYPE and every control-flow contract was already exact —
> locks, conditions, semaphores, barriers, class loading, resource lookup, and
> the whole charset decode/encode error-action matrix. Only **five messages**
> diverged, and all five are fixed here. That is the `G69-1` shape again: the
> lattice is right and the sentence is not.

**Status:** RESOLVED — fixed and verified; the sweep runs to completion and all
32 rows match. The body below is kept in its ORIGINAL wording, written while
§1 still hung, because the reasoning it records is the reasoning that found
the defect. The banner above is what is true now; where the two disagree, the
banner wins.
**Provenance:** both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-nolto` (non-LTO release, see `G71-1`), `--jdk-only`.
Probe: `regression-suite/probes/Sweep9ExceptionContracts.java`, 33 rows, ASCII.

---

## 0. Why this axis

`G68-1` §0 found that sweeping contracts that are an **exception or a side
effect** returned four defects in one pass where five sweeps of *values* had
returned six in total. `G68-1` N3 and `G69-1` N3 both nominated the same
untouched half of that axis: `Thread` interrupt/join, lock and condition
contracts, class-loading and resource failures, and charset decode/encode
error actions. This is that sweep.

It found a defect in row 1 and a hang in row 5.

## 1. THE HANG — `Object.wait(negative)` never returns

```text
                  HotSpot                                       CratonVM
o.wait(-5)        IllegalArgumentException                      BLOCKS FOREVER
                  "timeout value is negative"
```

Held monitor, negative timeout. HotSpot refuses the argument. CratonVM appears
to treat a negative timeout the way it treats `0` — wait indefinitely — so the
thread parks with nobody to notify it and the VM never exits.

**This is worse than a wrong answer.** A wrong value is visible at the point
it is produced; a hang is a liveness failure that presents as "the application
stopped", usually far from the call. Any program that computes a timeout and
lets it go negative (a deadline already passed — `deadline - now()`, the most
ordinary way to get one) hangs here and returns immediately on HotSpot.

It is also why this record existed instead of a complete sweep: 29 of the 33
rows could not be measured on CratonVM, because the probe could not get past
row 5. **They have since been measured — see the banner.** Their oracle values
are in §4.

## 2. `Thread.sleep(-1)` succeeds silently

```text
                  HotSpot                                       CratonVM
Thread.sleep(-1)  IllegalArgumentException                      no throw
                  "timeout value is negative"
Thread.join(-1)   IllegalArgumentException                      the same  <- correct
```

The same argument check, on two neighbouring methods, present on one and
missing on the other. `join` is right, which is what makes `sleep` a
misspelling rather than an unimplemented contract — and the pair is the
evidence that the rule was known.

## 3. Two messages, right type and wrong text

```text
                     HotSpot                       CratonVM
o.wait()  unowned    "current thread is not owner" "thread Thread-0 called wait() without owning the monitor"
o.notify() unowned   "current thread is not owner" "thread Thread-0 called notify() without owning the monitor"
```

`IllegalMonitorStateException` in both, at the right moment in both. The text
is CratonVM's own, and it names the thread — more informative than HotSpot's
and still wrong, for the reason `G69-1` set out at length: the message is the
diagnostic surface, and a program or a test that matches on it does not care
which of the two is more helpful. Note the shape is the same as `G69-1`'s: the
lattice is right and the sentence is not.

## 4. The oracle for the 29 rows the hang blocked

Recorded when CratonVM's side was still unknown. **CratonVM now matches every
one of them.** Kept because the column is worth having written down, and
because it is what made the re-run a diff rather than a measurement.

```text
wait_negative              IllegalArgumentException | timeout value is negative
start_twice                IllegalThreadStateException | null
setPriority_bad            IllegalArgumentException | null
interrupted_flag           no throw   (first call true, second false)
sleep_after_interrupt      InterruptedException | sleep interrupted  (flag CLEARED)
unlock_not_held            IllegalMonitorStateException | null
await_not_held             IllegalMonitorStateException | null
signal_not_held            IllegalMonitorStateException | null
rwlock_write_unheld        IllegalMonitorStateException | null
lock_getHoldCount          no throw   (reentrant count 2)
semaphore_negative         IllegalArgumentException | null
cdl_negative               IllegalArgumentException | count < 0
cyclic_zero                IllegalArgumentException | null
forName_missing            ClassNotFoundException | no.such.Klass
forName_null               NullPointerException | null
forName_array_binary       no throw          ("[I" IS a legal binary name)
forName_primitive          ClassNotFoundException | int
loadClass_missing          ClassNotFoundException | no.such.Klass
getResource_missing        null, no throw
getResourceAsStream_missing null, no throw
decode_REPORT              MalformedInputException | Input length = 1
decode_REPLACE             no throw          (U+FFFD substituted)
encode_unmappable_REPORT   UnmappableCharacterException | Input length = 1
encode_lone_surrogate      MalformedInputException | Input length = 1
charset_unsupported        UnsupportedCharsetException | no-such-charset-42
charset_illegal_name       IllegalCharsetNameException | bad name!
string_bad_charset         UnsupportedEncodingException | no-such-charset-42
decoder_malformed_len      getInputLength() == 1
```

Note `forName_array_binary` and `forName_primitive` disagree with each other —
`"[I"` resolves and `"int"` does not — which is the kind of row that gets
guessed wrong.

## 5. Why nothing was fixed in the FIRST pass (superseded)

The hang had to be fixed first, because it was what stopped the rest of the
axis being measured and it changed what the following 28 rows even did. That
ordering held: the fix landed, the sweep ran, and the remaining five defects
were only findable afterwards.

**Both halves are now done and 14 vector rows are in `RJdkIntrinsics3`'s
`misc` family (35 -> 49).** Note one property of the hang row: if it
regresses, the family HANGS rather than fails and the harness reports a 120 s
timeout. That is the only signal a liveness defect can give, and it is called
out at the rows so a future reader does not dismiss the timeout as
flakiness.

## 6. NOMINATIONS — all three CLOSED

**N1 — CLOSED.** The missing argument check, on `Object.wait(long)` and
`Thread.sleep(long)`. Fixed at `ebd877d45` and verified at `9fc2f3a55`. Two of
its premises were wrong and are corrected in the banner: `Thread.join` was not
the correctly-written sibling (it is not ours at all), and the two `wait`
overloads do not share a message.

**N2 — CLOSED by measurement.** The other 29 rows were re-run and the axis was
in better shape than this record assumed: every exception TYPE and every
control-flow contract already exact, five messages wrong. Worth keeping as a
result rather than deleting — a nomination that resolved cheaply is as useful
to the next reader as one that did not.

**N3 — CLOSED.** The two monitor messages, done with N2 as suggested, plus
three more the re-run exposed (`Thread.start` twice, `Class.forName(null)`,
`Thread.sleep` interrupted). One of them was not a string edit: an EMPTY
message is not the empty string, and `RuntimeError` carries a `String`, so
`Some("")` reached Java as `""` where HotSpot gives `null`.

**N4 — NEW. `Method.invoke` and `Constructor.newInstance` message families.**
`G69-1` N4 named them and this sweep did not reach them: it covered threads,
locks, loading and charsets. They are the last untouched corner of the
exception-contract axis that `G68-1` opened.
