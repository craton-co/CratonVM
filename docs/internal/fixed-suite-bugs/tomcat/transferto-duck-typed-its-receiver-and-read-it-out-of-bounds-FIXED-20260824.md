# `InputStream.transferTo` duck-typed its receiver and read it out of bounds

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-24, `native-builtins/src/phases_late/zip_streams.rs` (one condition) |
| **Severity** | medium as observed (out-of-bounds READS that fell back safely); high in principle (the same probe gates a WRITE) |
| **Signature** | `zgc real: field index OOB index=1..4 num_slots=0 op="get"/"set"` |
| **Before** | 16 hits per run of `TestDefaultServletRfc9110Section13`, 8 per run of `TestWebdavServletOptionsUnknown` |
| **After** | **0 and 0** |

## The defect

`native_input_stream_transfer_to` has a fast path for
`ByteArrayInputStream` — draining one is just advancing `pos` to `count`. It
recognised its receiver by **reading slots 0, 1 and 3** and checking whether
they looked like `(buf, pos, count)`:

```rust
let byte_array_stream_layout = matches!(
    (ctx.get_field(input, 0), ctx.get_field(input, 1), ctx.get_field(input, 3)),
    (Value::Object(Some(_)), Value::Int(_), Value::Int(_))
);
```

No class check. `org.apache.catalina.connector.CoyoteInputStream` declares
exactly one instance field (`ib`), and its supertypes `ServletInputStream` /
`InputStream` declare none — so slots 1 and 3 do not exist, and **the probe
itself is the out-of-bounds access.**

The heap returns a default for an out-of-range slot, so the match simply failed
and the call fell through to the correct slow path. That is why this survived:
it is silent, and both tests that carry it **pass**.

The dangerous half is the other direction. Had the probe ever matched on a
wrong class, three lines later:

```rust
ctx.set_field(input, 1, Value::Int(count));
```

writes a **primitive into whatever slot 1 is on that class** — a reference
field, in general. That is exactly the punned cell behind
`inline-getfield-read-a-non-reference-cell-as-a-pointer`, where a `[C` field
holding `Int(1)` made a compiled `arraylength` dereference the integer 1.

## Fix

Ask the class first, and keep the layout probe as a second condition so a future
field reordering degrades to the slow path instead of writing the wrong slot:

```rust
let input_is_byte_array_stream = ctx
    .class_name_arc_of_id(ctx.class_id_of_object(input))
    .as_deref() == Some("java/io/ByteArrayInputStream");
let byte_array_stream_layout = input_is_byte_array_stream && matches!(/* … */);
```

This is what the surrounding comment always meant — it says "for a
ByteArrayInputStream" — and what `dis_fast_window` in the same tree already does
for the same two stream shapes.

## How it was found, and the two readings that were wrong

`gc/src/zgc.rs` documents `num_slots=0` as the fingerprint of **a stale pointer
into an object compaction moved away**. Taking that at face value cost a session:
five heap sizes down to 192 MB, forty runs, no warning and no crash.

The corpse reporter's `None` branch printed `in_registry` but not the header
behind the address. Extended to print the class when the address is live, every
hit said the same thing:

```text
in_registry=true  class=org/apache/catalina/connector/CoyoteInputStream
header_num_slots=1  index=1..4  op=get/set
```

A **live registered object with an intact header and one real slot** — not a
corpse, not a dangling reference. The accessor was wrong, not the object.

The second wrong reading was the suspect. `BufferedInputStream`'s synthetic
natives hardcode exactly this shape (`in=0, buf=1, pos=2, count=3`), and
`BufferedOutputStream` beside them resolves its slots at runtime *because* the
real layout differs — a textbook twin asymmetry. **They are not the reader**:
those overrides were dropped (`_bis_dropped_overrides`), so the registration
never happens. A registration proves nothing until its registrar runs.

What actually named it was the report's own backtrace — and the fact that it
took a debugger to notice is a mistake worth recording.

`report_corpse_read` prints `backtrace=`, and on Linux that field carried **40
fully symbolized frames with file:line**, naming the defect four frames up:

```text
  1: check_field_index                gc/src/zgc.rs:8059
  2: get_field                        gc/src/zgc.rs:11594
  3: get_field                        vm/src/vm/vm_exec.rs:12237
  4: native_input_stream_transfer_to  native-builtins/.../zip_streams.rs:1532:17
```

That was in the log from the first run. I did not see it because `Display` is
multi-line: I printed the matching line, and a one-line view of a multi-line
field shows frame 0 and nothing else. I read that as "one frame", concluded the
in-process capture was broken under fat LTO, and went to `gdb` — which returned
the same chain the log already had.

Two comments in the tree encouraged that reading, and both are false on Linux:
`gc_quiescence::native_rvas` said `std::backtrace::Backtrace` "is useless in
this tree's release profile — fat LTO plus `debug = "line-tables-only"` renders
every frame `<unknown>`", and `report_corpse_read` pointed at it. This profile
sets `panic = "unwind"`, so `.eh_frame` is emitted and the unwinder walks
normally; `line-tables-only` is exactly what a backtrace needs. Both comments
are corrected, and `install_native_rva_hook` — which has no caller anywhere — is
now documented as Windows-only machinery that Linux does not need.

## Regression test

`the_class_gate_precedes_any_indexed_read_of_the_receiver` — a **source** guard,
deliberately. Reproducing this needs a receiver of a real Tomcat class, and this
crate has no mock `NativeContext` that can carry one. What can be pinned is the
ORDER: the class gate must appear before the first indexed read of `input`.

Negative control (gate removed, test kept):

```text
native_input_stream_transfer_to no longer asks the receiver's class before
probing its layout; a stream with fewer slots than the ByteArrayInputStream
shape is read out of bounds
```

The guard splits its own search literals so it cannot match its own source.

## Verification

| class | before | after |
|---|---:|---:|
| `catalina.servlets.TestDefaultServletRfc9110Section13` | 16 OOB | **0** |
| `catalina.servlets.TestWebdavServletOptionsUnknown` | 8 OOB | **0** |

Both still pass (`OK (1210 tests)` / `OK (104 tests)`). The counts are
load-independent, which matters: the host was at load 25–205 throughout, so no
wall-clock claim is made here.

`cratonvm-native-builtins`: **4158 passed / 2 failed on this branch and 4158 /
2 on `origin/dev`** — the two failures (`shared_secrets_bridge::tests::
representative_method_registered_per_owner` and one other) are pre-existing on
dev, reproduce 3/3 in isolation there, and are untouched by this change.

## Residual

This fixes the reader that the backtrace named. Whether it is the *only* reader
of the `num_slots` OOB signature across the suite is not established — these two
classes are the ones that carry it today, and both are now clean.

## Was this also the WRITER of the punned cell? Consistent, not observed

The crash that started this family was a `[C` field, `SQLChar.rawData`, holding
`Value::Int(1)` — a primitive in a reference slot. This native writes exactly
that shape:

```rust
ctx.set_field(input, 1, Value::Int(count));   // slot 1
```

and `rawData` **is** slot 1. The pre-fix gate required only slots 0/1/3 to look
like `(Object, Int, Int)`, which a `SQLChar` satisfies whenever `value` (slot 0)
holds a String and `rawData` (1) and `cKey` (3) are still zero-filled — a
zero-filled cell decodes as `Int(0)`.

Interleaved A/B on `TestWebdavPropertyStore`, same host, arms alternated per
run, counting dangerous punned cells (non-zero payload under a non-`Object`
tag):

| arm | runs | punned |
|---|---:|---:|
| before this fix | 93 | **3** |
| after it | 93 | **0** |

All three pre-fix hits are byte-identical: `class_id=1849 num_slots=8
field_index=1 tag=0 payload32=0x1 payload64=0x1 decoded=Int(1)`.

**That is consistent, not proof.** Against a 3-in-93 base rate, 0 in 93 is about
a 1-in-20 coincidence — the same strength as the crash A/B, and quoted the same
way. The decisive experiment is the writer-side watch added alongside this
(`CRATONVM_DBG_WATCH_PUN=<class-substring>:<slot>`, which prints a backtrace at
the store), run on a binary that still has the bug. That build was attempted
twice and **OOM-killed both times** (`signal: 9`, once at full parallelism and
once at `-j 2`) on a shared host running 23–30 GB of other people's work. So the
watch is in the tree, wired and ready, and the observation is not made.

If picking this up: revert the class gate, build with `-j 2` when
`free -g` shows headroom, and run the concurrent recipe with
`CRATONVM_DBG_WATCH_PUN=SQLChar:1`. One firing settles it.
