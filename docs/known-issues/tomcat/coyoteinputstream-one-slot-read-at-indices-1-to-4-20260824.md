# A live `CoyoteInputStream` with ONE slot is field-accessed at indices 1–4

| | |
|---|---|
| **Status** | OPEN — named and reproducible, NOT fixed (2026-08-24) |
| **Signature** | `zgc real: field index OOB index=1..4 num_slots=0 op="get"/"set"` |
| **Reproduces** | 100% solo: `catalina.servlets.TestDefaultServletRfc9110Section13` (16 hits), `catalina.servlets.TestWebdavServletOptionsUnknown` (8) |
| **Why it matters** | `CoyoteInputStream` is on the stack of the `TestSwallowAbortedUploads` SIGSEGV, and this signature accompanied that crash |

## What it is NOT

`gc/src/zgc.rs` documents `num_slots=0` as the fingerprint of **a stale pointer
into an object compaction moved away** — the vacated span is zeroed on purpose,
which erases the class id and the slot count. That reading cost an earlier
session hours pointed at the collector: five heap sizes down to 192 MB and forty
runs produced neither the warning nor a crash.

It is the wrong reading. With the corpse reporter extended to print the header
when the address is live, every hit says:

```text
in_registry=true  class=org/apache/catalina/connector/CoyoteInputStream
class_id=2651     header_num_slots=1    index=1 (and 2, 3, 4)    op=get/set
```

`in_registry=true` — the address is a **live, registered object**, not a corpse.
Its header is intact, its class is known, and it genuinely has **one** slot. So
the defect is not a dangling reference: something reads and writes slots 1
through 4 of an object that has one. `num_slots=0` in the warning line is the
*bounds* the accessor was given, not the object's real slot count.

`CoyoteInputStream` declares a single instance field (`ib`), and its supertypes
`ServletInputStream` / `InputStream` declare none, so `num_slots=1` is correct.
The accessor is wrong, not the object.

## What was ruled out

`java/io/BufferedInputStream`'s synthetic natives hardcode exactly the shape
being read — `in=0, buf=1, pos=2, count=3` (`BIS_FIELD_*`,
`native-io/src/lib.rs`) — and `BufferedOutputStream` beside them already
resolves its slots at runtime (`bos_slots()`) *because* the real-JDK layout
differs. That asymmetry made them the obvious suspect.

**They are not the reader.** Those overrides were dropped:

```rust
let _bis_dropped_overrides = "java/io/BufferedInputStream";
```

so the registration never happens and the constants are unreachable on this
path. A registration proves nothing until its registrar runs in this mode, and
this one does not run at all.

`dis_fast_window` reads `pos`/`count` off an inner stream, but it gates on
`class_name == "java/io/BufferedInputStream" || "java/io/ByteArrayInputStream"`
and uses `get_field_by_name`, so it is safe by construction.

## Next step

The reader is not identified, and guessing at it is what produced the wrong
suspect above. The corpse report carries a `backtrace=` field, but
`gc_quiescence::native_rvas()` came back empty on this path, so it fell back to
`Backtrace::force_capture` — one frame in the release profile. Making
`native_rvas` populate here, or symbolising the RVAs through
`CRATONVM_SYMBOLIZE` against the same binary, turns the signature into a named
call site.

It reproduces 100% of the time on two classes with no concurrency and no special
heap, so this is a short step rather than a hunt.

## Reproduction

```bash
CRATONVM_DBG_ZGC_CORPSE=1 CRATONVM_DBG_LAYOUT=1 \
  <cratonvm> --java-home <jdk25> --Xmx 2g -c "$CP" \
  org.junit.runner.JUnitCore \
  org.apache.catalina.servlets.TestDefaultServletRfc9110Section13
```

The class PASSES; the signature is in its stderr. Do not wait for a failure.
