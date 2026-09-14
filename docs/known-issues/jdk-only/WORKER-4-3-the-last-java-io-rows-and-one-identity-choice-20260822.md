# WORKER-4-3 — the `java.io` rows nobody had reached, and the one class-identity choice that five symptoms hang off

**Status: MEASURED, five defects fixed, one refusal with the measurement that
would settle it.** Lane WORKER 4, 2026-08-22. Third in the series after
`WORKER-4-1` (twelve fabricated abstract receivers) and `WORKER-4-2` (the
`java.io` census and its first fourteen retirements).

**Provenance.** Linux (Azure host 2), Temurin **25.0.4+7**, worktree
`/data/cvm-w4io2` cut from `claude/jdk-only-mode-handoff-09b48c` at
`d235cced9`. `base` is that worktree built at the pre-change commit, so every
"before" is a real run of a real binary. Probes:
`regression-suite/probes/W4Buffers.java` (80 cases),
`W4Files.java` (40) — both diffed against the oracle in both modes.

---

## 1. Why these two probes

`WORKER-4-2` §8 named what its four probes had NOT reached: roughly 74 owned
§1.4 shadow rows outside `File` / `PrintStream` / `Data*Stream` / `Scanner`, and
four class-identity divergences left as nominations. This closes both lists.

`W4Buffers` covers `ByteArrayOutputStream` (11 rows), `ByteArrayInputStream`
(8), `BufferedWriter` (6), `FilterOutputStream` (5), `FileOutputStream` (4),
`FilterInputStream` (2), `BufferedInputStream` (1), `FileDescriptor` +
`FileDescriptor$1` (11), and the `java.io` exception classes (10). **80 cases,
three defects.**

`W4Files` asks `WORKER-4-1` N3's four identity divergences BEHAVIOURALLY —
what can an application actually tell? **40 cases, seven diffs, which turn out
to be two defects and one decision.**

---

## 2. `BufferedInputStream.skip` returned the right COUNT and left the stream in the wrong PLACE

The sharpest of the three, because it produced **wrong bytes** with every
individual step agreeing.

```text
BufferedInputStream(bais, 4) over "0123456789", then
read(); mark(8); read(buf,0,4); reset(); read(); skip(3); readAllBytes()

  read           48        agreed
  read(buf,0,4)  4:1234    agreed
  reset(); read  49        agreed
  skip(3)        3         agreed
  readAllBytes   "56789" on HotSpot,  "23489" on CratonVM
```

### 2.1 One object, two cursors

`mark`, `reset`, `read` and `readAllBytes` are **not registered** on
`BufferedInputStream` in this build — they run the JDK's own bytecode over the
real `buf` / `pos` / `count` / `markpos` fields. **`skip` was**, and it drove a
completely different model: a side table of replayed bytes keyed off the
object, falling through to `invoke_virtual(input, "read", "()I")` on the
UNDERLYING stream.

So `skip(3)` pulled three bytes out of the SOURCE (index 5 → 8) while the JDK's
own `buf` still held `"234"` unread. `readAllBytes` then returned the buffer's
leftovers followed by the source's new position: `"234"` + `"89"`. **The count
was right because three bytes really were consumed — from the wrong place.**

This is `[nat hidden]` in its sharpest form. The usual shape is a native and
bytecode disagreeing about a VALUE. This is a native and bytecode maintaining
two independent **cursors** over one stream, where every observable except the
final read agrees.

### 2.2 Retired, and trap 4 is why it is safe rather than in spite of it

`BufferedInputStream extends FilterInputStream`, and
`java/io/FilterInputStream.skip(J)J` IS registered, so the superclass walk
PROMOTES it. That is the outcome, and it is the right one:
`filter_input_stream_skip` discards through
`invoke_virtual(this, "read", "([BII)I")` on the RECEIVER — the buffered
stream's own real `read` over its own real buffer. It cannot desynchronise the
two cursors because it only ever moves one.

**A promotion is not automatically a hazard.** `H22` was right to check every
one; the check can also come back "the loser is the correct body", and this is
that case.

---

## 3. `ByteArrayOutputStream.toString("no-such-charset")` returned `""`

```text
  HotSpot    UnsupportedEncodingException: no-such-charset
  CratonVM   ""
```

A fabricated success, and the worst available one: the caller cannot distinguish
an unusable charset from an empty buffer.

Both `toString(String)` and `toString(Charset)` shared one body. That is fine
for the decode and wrong for the contract — the two overloads differ in exactly
one way, which is that the NAME form can be handed something that does not
exist. It now has its own body.

**The validity check asks the JDK, not our own table.** `native-api`'s
`canonical_charset_name` knows about forty names; a JDK image ships around a
hundred and seventy. Refusing everything the table does not list would turn "we
have not written this alias down" into an application error — a worse failure
than the one being fixed. `Charset.isSupported(String)` lives in the image, is
the JDK's own predicate, and cannot drift from it. If the call itself cannot be
made, the answer is "supported" and the old decode runs: a correctness fix must
not become a new way to fail.

---

## 4. `FileNotFoundException` carried the path and not the reason

```text
  HotSpot    /definitely/not/here (No such file or directory)
  CratonVM   /definitely/not/here
```

HotSpot's platform layer builds this message as the path followed by
`strerror(errno)`, and it is the entire content of the log line an operator
reads. A bare path says "something went wrong with this file" — it does not
distinguish absent from unreadable from is-a-directory, which is precisely the
distinction being looked for.

**The file already knew this.** `reject_directory_open` has rendered
`<path> (Is a directory)` all along, with a comment explaining that HotSpot's
`handleOpen` does exactly that. One arm of one convention had it; the default
arm did not.

Call sites that hold the `io::Error` now use `file_not_found_because`, so the
reason is the REAL errno (`Permission denied` on an unreadable file) rather than
the assumed default.

---

## 5. `readAttributes(p, BasicFileAttributes.class)` handed back a POSIX view

`WORKER-4-1` N3 recorded this as a class-name difference and left it as
cosmetic. It is not, and the probe is the difference between the two claims:

```text
  Files.readAttributes(f, BasicFileAttributes.class) instanceof PosixFileAttributes
    HotSpot    false
    CratonVM   true

  ((PosixFileAttributes) basic).permissions()
    HotSpot    unreachable — the cast fails
    CratonVM   reachable:true
```

`basic_file_attributes_alloc` hands back a real `sun.nio.fs.UnixFileAttributes`,
which implements `PosixFileAttributes`. HotSpot hands back
`UnixFileAttributes$UnixAsBasicFileAttributes`, a wrapper implementing ONLY
`BasicFileAttributes`.

The direction that bites is not "an application can read permissions it should
not have". It is that a `basic instanceof PosixFileAttributes` branch — the
standard way to ask *"is this filesystem POSIX?"* — **silently takes the POSIX
path on a request that asked for basic**.

Fixed with the JDK's own narrowing factory,
`UnixAsBasicFileAttributes.wrap(...)`, rather than a hand-built wrapper: its
delegating bodies call `attrs.isDirectory()` and friends, which are the
accessors this bridge already registers. **Only the two `Class`-taking entry
points narrow.** Every internal consumer — `FileTreeWalker`, the Dos view,
`getLastModifiedTime` — reads the carrier through the accessor helpers and needs
the full object.

---

## 6. REFUSED — `Files.newInputStream` returns a `FileInputStream`, and five symptoms hang off that one choice

The remaining five diffs are not five defects. They are one decision, observed
five ways:

```text
  Files.newInputStream(f)                     CratonVM java.io.FileInputStream
                                              HotSpot  sun.nio.ch.ChannelInputStream

  in instanceof FileInputStream               HotSpot false   CratonVM true
  ((FileInputStream) in).getChannel()          HotSpot unreachable  CratonVM channel-size=10
  read() after close()                        HotSpot ClosedChannelException  CratonVM IOException
  Files.newOutputStream(f) instanceof FOS      HotSpot false   CratonVM true
  write() after close()                       HotSpot ClosedChannelException  CratonVM IOException
```

**The two exception rows are NOT independently fixable, and trying would be a
fabrication.** `FileInputStream.read()` after `close()` throwing
`IOException: Stream Closed` is the CORRECT answer *for a `FileInputStream`*.
Making our object throw `ClosedChannelException` while remaining a
`FileInputStream` would produce a combination no JDK ever produces. The
exception type is downstream of the identity; fix the identity or leave both.

**Why this is refused rather than fixed.** Unlike every other divergence this
lane closed, no answer here is WRONG:

* the object is a CONCRETE class either way, so JVMS §6.5 is satisfied and
  `W4Abstract` stays at 0 of 63;
* both `instanceof` branches lead to correct code — one takes a channel fast
  path, the other a stream path;
* nothing returns wrong data, and no `catch` clause fails to match that would
  have matched (`ClosedChannelException extends IOException`).

Against that, `Files.newInputStream` is on the path of `Files.readAllBytes`,
`Files.lines`, jar and class-file reading, and most of the boot sequence.
Re-homing it onto a `ChannelInputStream` means minting that class, giving it a
real `ReadableByteChannel`, and mirroring the read/close surface onto it —
`WORKER-4-1` §3's three halves, on the busiest stream factory in the tree, for
`instanceof` fidelity.

**What would settle it**, and what a lane picking this up should measure first:
an application that takes the `instanceof FileInputStream` fast path and gets a
DIFFERENT ANSWER, not merely a different route. This lane looked and did not
find one. Until then this is a divergence with a stated cost, which is a
different thing from a defect.

`[a refusal with evidence beats a retirement without it]`.

---

## 7. Acceptance, and the one red vector that is not this lane's

| arm | base | after |
|---|---|---|
| `CRATONVM_ARGS=--jdk-only SUITE=all` | 107/107 | **107/107** |
| `SUITE=all` | 106/107 | 106/107 |
| `SUITE=core` | 66/67 | 66/67 |

Both new probes green: `W4Buffers` **80/80**, `W4Files` 40 cases with the five
diffs §6 refuses and no others. The six probes from the earlier records are
unchanged.

**`RTreeRangeGc` is red in the two COMPATIBLE arms, on the branch tip, without
this lane's commits.** `WORKER-1-NOTE-1` owns that vector and establishes its
shape — two defects under one name, a ~25% flake under `--jdk-only` and a
deterministic failure in compatible mode. `WORKER-4-NOTE-2` is a companion
carrying the three things this lane needed and that note does not have: the
standalone reproduction (`--real-jdk --Xmx 64m`, without which a plain run is a
green gate by `run.sh`'s own account), the assertion itself
(`headMap(k,false): 0 entries, expected 300`, which the harness line truncates
away), and an interleaved ten-run A/B clearing this lane — **base 5/5 red,
r1 4/5 red**, the same failure at the same rate on both sides.

### 7.1 A trap this lane walked into, stated because the brief's version is narrower than it reads

The first appearance of that failure came with two `HARNESS FAULT — MAIN CLASS
NOT FOUND` entries beside it — the brief's own trap-8 signature, *"total redness
that INCLUDES the harness guard is an ENVIRONMENT failure"*. It was, and the
environment was this lane: a single-vector control was launched through `run.sh`
while a full sweep was still running **in the same worktree**, and the second
run recompiled `regression-suite/build` underneath the first.

H0 PID-scoped `.guard-tmp` and the brief now reads *"FIXED — you may now sweep
concurrently"*. That is true **across worktrees**. `regression-suite/build` is
still one directory per worktree, so two `run.sh` in the SAME worktree still
collide. Every number in this record was re-measured afterwards with one sweep
at a time.

---

## 8. What this record does NOT claim

* **The `Files.walk` identity is untested for its consequence.** `walk` returns
  `ReferencePipeline$Head` where HotSpot returns `$3`. All eight behavioural
  cases agree — count, contents, depth limit, `onClose` running, the missing
  directory — so nothing in §6's argument is weakened, but the pipeline SHAPE
  difference (ours has no intermediate stage) was not probed for laziness or
  for short-circuit behaviour on a huge tree.
* **Windows was not run.** Both new probes are Linux-only measurements, and §5's
  narrowing deliberately does nothing on a Windows carrier because that image
  has no equivalent nested wrapper.
* **`FileDescriptor$1`'s 10 shadow rows are covered only indirectly.**
  `W4Buffers` reaches them through `getFD()` / `valid()` / `sync()`; the
  `JavaIOFileDescriptorAccess` shared-secret surface itself is not driven.

---

## 9. Index rows (for H0 to move into `INDEX.md`)

* `WORKER-4-3` — the last `java.io` rows put to the oracle: 120 cases, five
  defects. `BufferedInputStream.skip` kept a SECOND cursor over a stream whose
  other methods run real bytecode, so the count was right and the next read
  returned the wrong bytes; `ByteArrayOutputStream.toString(badName)` returned
  `""` instead of throwing; `FileNotFoundException` carried no reason;
  `readAttributes(p, Basic.class)` handed back a POSIX view, so
  `instanceof PosixFileAttributes` answered true. And ONE refusal:
  `Files.newInputStream` returns a `FileInputStream` where HotSpot returns a
  `ChannelInputStream`, which is five measured symptoms of a single identity
  choice in which no answer is wrong. Also carries the reproduction for
  `RTreeRangeGc`, red on the tip in compatible mode and NOT this lane's
  (`WORKER-4-NOTE-2`), and the narrower reading of trap 8 that cost this lane a
  contaminated run: concurrent sweeps are safe across worktrees, not within
  one.
