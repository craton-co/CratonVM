# A `.jfr` file CratonVM writes cannot be parsed by the JDK's own `RecordingFile`

**Status:** ✅ FIXED 2026-08-13, on branch
`fix/netty-jfr-known-issues-20260813` (from `origin/dev` `bd80019c8`). Verified
on the same Azure Linux host (`20.80.105.49`) against the JDK's own
`RecordingFile`, `jfr summary` and `jfr print`.

| | before | after |
| --- | --- | --- |
| empty `Recording` → `dump` → `readAllEvents` | `IOException: Unknown string encoding 17` | reads, 1 event |
| one custom event → `dump` → `readAllEvents` | `IOException: Unknown string encoding 17` | reads, `ProbeEvent` with `capacity=4096`, `label=direct-buffer`, `pooled=true`, `stamp=1234567890123`, `ratio=0.75` — every field matching the HotSpot arm |
| `jfr summary <file>` | rejected | `Version: 2.1`, `Chunks: 1`, per-type counts |
| `jfr print <file>` | rejected | prints |

**What was wrong** — CratonVM had exactly one `.jfr` writer, and it wrote a
CratonVM-specific format that shares only the `FLR\0` magic and the `2.x`
version with the JDK's. Every framing rule after that disagreed, which is why
the JDK's parser was mis-framed from the first record and reported a
string-encoding tag it had never read as one. The disagreements, each now
named in `jfr/src/jdk_chunk.rs` against the JDK class that enforces it:

* the header was **72 bytes**, not 68 (`ChunkHeader.HEADER_SIZE`);
* `file_state` used **1 for complete**, the exact inverse of the JDK's
  `finished == 0` (`ChunkHeader.refresh`);
* integers were **zigzag** LEB128, where the format is plain unsigned LEB128
  with a 9-byte cap whose last byte is raw (`RecordingInput.readLong`);
* the metadata section was a bespoke layout, not the JDK's string-pool +
  element-tree metadata event (`MetadataReader`);
* the checkpoint section was a bespoke string pool, where the JDK expects
  either a real checkpoint event or `constantPoolPosition == 0`.

**The fix** — a second writer, `jfr/src/jdk_chunk.rs`, that emits the JDK's
format, and every operator-visible dump switched onto it:
`FlightRecorder::dump_recording` (which is what `jdk.jfr.Recording.dump`, the
`JFR.dump` diagnostic command and the CLI's exit-time dump all go through) and
`phase::write_jfr_report`. The original writer stays in `jfr/src/dump.rs` for
the internal round trip its own reader (`read_events`) still serves; the two
formats and why they coexist are documented at the top of `jdk_chunk.rs`.

Three things the fix had to get right that are not obvious from the symptom:

1. **An empty recording must still produce a parseable chunk.** The metadata
   event is mandatory (`metadataPosition == 0` makes `ChunkHeader.refresh`
   reject the chunk as truncated), and `chunkSize` must equal the file size or
   `isLastChunk()` never terminates.
2. **`constantPoolPosition == 0` is legal** and means "no constant pools", so a
   writer that models no constant-pool-backed field needs no checkpoint event at
   all. That is what keeps this writer small.
3. **A `Null` field value must occupy its declared width.** One 1-byte null in a
   `double` slot mis-frames every field after it — the same class of defect as
   the original.

**Residual, by design:** event types are written with `startTime`, `duration`
and the declared payload fields only — no `eventThread`, no `stackTrace`. Both
are constant-pool references in the JDK's format and CratonVM records neither
for the events that reach this writer, so consumers see
`RecordedEvent.getThread() == null`, which is the same answer the JDK gives for
its own event types that omit those fields.

## Original report (2026-08-12)

Found while probing the JFR gap behind netty investigate-batch-02's
`JfrEventsTest`; filed as an **independent** defect — no netty test depends on
it, and fixing it does not fix `JfrEventsTest`. Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `6d1bfd531`.

## Symptom

`Recording.start()` → `commit()` → `stop()` → `dump(path)` all succeed and
produce a plausible file. Reading it back with the JDK's own parser fails:

```
java.io.IOException: Unknown string encoding 17
        at jdk.jfr.consumer.RecordingFile...
```

## It is the chunk/metadata region, not event serialization

An **empty** recording — nothing enabled, nothing committed — is already
unreadable, which localises the defect away from the event writer:

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| empty recording → `dump` | 120 543 bytes, `readAllEvents` → 0 events | 7 690 bytes, **`IOException: Unknown string encoding 17`** |
| one custom event → `dump` | 121 199 bytes, `readAllEvents` → 1 event, fields correct | 7 796 bytes, **same `IOException`** |

Same message, same position in both, with and without events. So the parser is
already mis-framed before it reaches any event data.

"Unknown string encoding 17" is the JDK reader rejecting a byte where it
expects a string-encoding tag — the valid set is 0 `NULL`, 1 `EMPTY_STRING`,
2 `CONSTANT_POOL`, 3 `UTF8_BYTE_ARRAY`, 4 `CHAR_ARRAY`, 5 `LATIN1_BYTE_ARRAY`.
A 17 is not "we chose encoding 17"; it means the reader's cursor is at the
wrong offset, so the byte it read is some other field's data. Look for a
length/offset written with the wrong width or a missing field in the chunk
header or the metadata event, not for a string-encoding branch.

The ~16× size difference (7.7 KB vs 120 KB) is expected and not itself the bug:
HotSpot's chunk carries the full JDK event-type metadata, CratonVM's carries
only what it models. It does mean a byte-diff against a HotSpot chunk will not
be useful; compare against the JDK's chunk **format** instead.

## Impact

Anything that dumps a recording and expects a standard `.jfr`: JMC, `jfr
print`/`jfr summary`, CI flight-recorder artefacts, and any library that
round-trips its own events through `RecordingFile`. Silent until something
tries to read the file — the write side reports success throughout.

Also worth knowing when working on the retired
`jfr-recordingstream-delivers-no-events` write-up: the streaming gap cannot
be closed by "write chunks to disk and let `EventDirectoryStream` poll them",
because the chunks CratonVM writes are not parseable. Fix this one first if
that route is ever attempted.

*(2026-08-13 note: this writer now makes that route* possible *— the chunks are
parseable — but it is still not the route the streaming fix took. See that
write-up's "Why not the repository route" section.)*

## Repro

```java
Path f = Files.createTempFile("p3", ".jfr");
Recording r = new Recording();
r.start(); r.stop(); r.dump(f);
System.out.println(Files.size(f));
RecordingFile.readAllEvents(f);      // IOException on CratonVM, fine on HotSpot
```

```bash
javac -d . JfrProbe3.java
java     -cp . JfrProbe3    # emptyRecording: read=count=0
cratonvm --java-home <jdk25> -cp . JfrProbe3
                            # emptyRecording: read=THREW ... Unknown string encoding 17
```
