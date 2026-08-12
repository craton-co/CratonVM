# A `.jfr` file CratonVM writes cannot be parsed by the JDK's own `RecordingFile`

**Status:** OPEN (2026-08-12). Found while probing the JFR gap behind
[netty investigate-batch-02](investigate-batch-02.md)'s `JfrEventsTest`; this is
an **independent** defect — no netty test depends on it, and fixing it does not
fix `JfrEventsTest`. Azure Linux host (`20.80.105.49`), binary built from
`origin/dev` `6d1bfd531`.

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

Also worth knowing when working on
`jfr-recordingstream-delivers-no-events-20260812.md`: the streaming gap cannot
be closed by "write chunks to disk and let `EventDirectoryStream` poll them",
because the chunks CratonVM writes are not parseable. Fix this one first if
that route is ever attempted.

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
