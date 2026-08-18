# G72-1 — a negative timeout that waits forever

**Status:** MEASURED, **NOT FIXED**. The sweep is INCOMPLETE and cannot be
completed until §1 is fixed — the VM blocks partway through it.
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

It is also why this record exists instead of a complete sweep: **29 of the 33
rows have never been measured on CratonVM**, because the probe cannot get past
row 5. Their oracle values ARE recorded in §4 so the next pass starts with
half the work done.

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

## 4. The oracle for the 29 unmeasured rows

Recorded so the next pass does not re-derive them. **These are HotSpot values
only** — CratonVM's side is unknown for every one.

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

## 5. Why nothing is fixed here, and no vector rows

The hang has to be fixed FIRST, because it is what stops the rest of the axis
being measured, and fixing it changes what the following 28 rows even do. A
partial fix landed now would also land a probe that cannot run to completion.

No vector rows: same standard as `G67-1` §4 and `G71-1` — a row for a defect
nobody is fixing turns the suite red and trains people to ignore it. The probe
is checked in so the rows can be lifted with the fix.

## 6. NOMINATIONS

**N1 — fix §1 and §2 together; they are one missing argument check.** Both are
"a negative timeout is not a long wait, it is an error". `Thread.join(long)`
already has it and is the in-tree exemplar. `Object.wait(long)`,
`Object.wait(long,int)` and `Thread.sleep(long)`/`sleep(long,int)` are the
sites. Fixing §1 is also what unblocks N2.

**N2 — re-run the sweep and measure the other 29 rows.** The probe is checked
in and the oracle column is already in §4, so this is a diff, not a
measurement exercise. Given `G68-1` returned four defects from 29 rows on this
axis, assume this half is not clean.

**N3 — the two monitor messages, §3.** Cheap, and in the same family `G69-1`
just finished for fields. Do it with N2 rather than alone.
