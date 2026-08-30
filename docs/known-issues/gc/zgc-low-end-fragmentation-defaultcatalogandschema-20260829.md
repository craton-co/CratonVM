# ZGC low-end arena fragmentation — `DefaultCatalogAndSchemaTest`, one 32-byte object walling a 21 KB window

## Status

**OPEN, 2026-08-29.** The one surviving occurrence from
`zgc-arena-fragmentation-occurrences-to-reverify-20260829.md`, which asked for a
four-arm re-verification against the 2026-08-29 repairs. That verification is
done and is recorded in
`internal/fixed-suite-bugs/gc/zgc-frag-occurrences-reverified-20260829.md`; this
page is what it left open.

The short version: this class fails at the **LOW end** of the arena, where the
2026-08-29 repair — a compactor for the LARGE-OBJECT end — does not apply by
construction. `hi_cycles=0 declined=0` on every arm says the high-end compactor
was never even offered a candidate, so no arm of that matrix could have moved
it and none did.

## The failure

`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`,
`--Xmx 1500m`, CratonVM `dev@96e07ca86` plus the self-recursion JIT fix on this
branch:

| VM | result |
|---|---|
| real HotSpot | **PASS** `found=132 ok=132 failed=0`, 77 s |
| CratonVM, all four arms | **no `@@RESULT`** — `OutOfMemoryError`, 4 per run |

| arm | frag gauge lines | OOMs | `hi_cycles` | `hi_declined` |
|---|---:|---:|---:|---:|
| default | 5 | 4 | 0 | 0 |
| `CRATONVM_ZGC_HIGH_COMPACTION=0` | 9 | 4 | 0 | 0 |
| `CRATONVM_ZGC_TLAB_STARVED_RECYCLE=0` | 1 | 0 | — | — |
| both `=0` | 1 | 0 | — | — |

## Which END the request came from, which is the first question

```text
zgc: arena allocation failed  request=16400  used=1572852104  capacity=1572864000
     free_list_bytes=922711608  largest_free_block=14232  free_spans=44298
     failure_seq=1
```

**`request=16400`**, and in a second run `request=21008`. The high end serves
requests `>= 65536`; both of these are LOW-end requests. That is the whole
reason the 2026-08-29 high-end compactor cannot help here, and it is why
`hi_cycles=0 declined=0` rather than `cycles=0 declined=N` — the compactor was
not offered a fragmented large-object end to decline, because the fragmentation
is not there.

880 MB is on the free list and the largest contiguous block is 14,232 bytes,
against a 16,400-byte request.

## The diagnostic already names the wall, and it is one object

```text
zgc frag: the CHEAPEST window that could serve this request — 32 live bytes in
1 run(s) are all that stand between 21240 free bytes spread over 21272 bytes of
contiguous arena.
     request=21008 window_bytes=21272 window_free=21240 wall_bytes=32 walls=1

zgc frag: wall occupant
     class=org/hibernate/type/format/jackson/JacksonJsonFormatMapper count=1 bytes=32
```

**One 32-byte `JacksonJsonFormatMapper` is holding a 21 KB window hostage.**
`walls=1`, `wall_bytes=32`: this is the "handful of survivors" end of that
line's own scale, not a live/dead mosaic. Relocating a single 32-byte object
would serve the request.

The whole-heap shape says the same thing at scale: `spans=1129201`,
`largest_span=14232`, `walls=1129200`, `wall_bytes=649082008` — over a million
free spans, each walled off from the next by a live object, and not one of them
16 KB wide.

## Why this is not the same defect as the page it came from

The parent page grouped this with a Spring Framework class under one "symptom to
match on". They are not the same defect:

* This one is a **low-end** allocation failure with an `OutOfMemoryError` and a
  named single-object wall.
* The Spring one hangs with the fragmentation gauge firing **zero** times in
  some arms, and is re-attributed in
  `simpleclienthttpresponse-hang-is-not-arena-fragmentation-20260829.md`.

## Not yet done

- Whether the low end can be compacted at all in the current design, which is
  the question `CRATONVM_ZGC_TARGETED_COMPACTION` was built for and measured as
  engaging zero times. `walls=1 / wall_bytes=32` is the most favourable input
  such a compactor will ever get; if it cannot take this window, that is worth
  knowing explicitly rather than by absence.
- Why `JacksonJsonFormatMapper` — a 32-byte singleton-shaped object — is sitting
  in the middle of the arena rather than being promoted or placed with other
  long-lived objects. The wall being ONE object of a class that should have ONE
  instance suggests placement, not volume.
- The `tlab0` / `both0` arms report `frag=1 oom=0`, i.e. they die EARLIER and
  differently from the default arm. Not characterised; the four-arm table above
  is a pass/fail matrix, not a cause matrix.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm --java-home <jdk25> --Xmx 1500m \
  -Duser.language=en -Duser.country=US -Djava.awt.headless=true \
  @common.args CratonRunner \
  org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
```

Read `zgc frag:` in the output before anything else — it names the wall
occupant, which is the question the bare `arena allocation failed` line cannot
answer.

**This class needs the self-recursion JIT fix on this branch to reach its own
failure at all.** Without it the compiler thread panics during Hibernate
bootstrap and the class runs to a 600 s timeout entirely interpreted, producing
no `@@RESULT` and no fragmentation lines — a different failure wearing the same
"no result" clothes.
