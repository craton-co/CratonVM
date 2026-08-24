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

What actually named it was a debugger. The in-process capture is useless here —
`gc_quiescence::native_rvas()` has **no installed hook at all**
(`install_native_rva_hook` has zero callers, and the machinery it was written
for is Windows `RtlCaptureStackBackTrace`), so it falls back to
`Backtrace::force_capture`, which yields one frame under fat LTO. `gdb` unwinds
what the process cannot:

```text
#0  report_corpse_read              gc/src/zgc.rs:8332
#1  check_field_index               gc/src/zgc.rs:8059
#2  get_field                       gc/src/zgc.rs:11594
#3  get_field                       vm/src/vm/vm_exec.rs:12237
#4  native_input_stream_transfer_to native-builtins/.../zip_streams.rs:1532
```

Line 1532 is the probe. One breakpoint, one backtrace, done.

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
