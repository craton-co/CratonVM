# Fix: `NoSuchMethodError: java/io/BufferedWriter.write([BII)V`

## Symptom
Running the JUnit Platform console launcher (`junit --help`, and the entire
Apache Commons Math JUnit5 suite) failed with:

```
java.lang.NoSuchMethodError: java/io/BufferedWriter.write([BII)V
```

`java.io.BufferedWriter` is a CHAR `Writer`: it has `write([CII)V` (char[])
and `write(Ljava/lang/String;)V`, but **no** `write([BII)V` (byte[]). The
bogus byte-array call was synthesized by CratonVM's own native print
intercept, not by JDK bytecode.

## Root cause
picocli writes help text via `java/io/PrintWriter.print(String)`. CratonVM
intercepts it natively:

`native_print_string` → `stream_write` → `route_write_through_out`
(`native-builtins/src/lib.rs`).

`route_write_through_out` reads the `PrintWriter`'s `out` field. For a
`PrintWriter(OutputStream)`, the JDK ctor wraps the sink as
`new BufferedWriter(new OutputStreamWriter(out))`, so `out` is a CHAR
`Writer` (BufferedWriter), **not** a byte `OutputStream`. The code then
unconditionally called `out.write([BII)V` → NoSuchMethodError.

The same defect existed in `native_printwriter_printf`'s byte fallback: it
tried `write(String)` first but fell through to `write([BII)V` on failure,
hitting the same missing method when the backing sink is a Writer.

## Fix (only `native-builtins/src/lib.rs`)
Added two helpers and a runtime-class guard:

- `sink_is_writer(ctx, out) -> Option<bool>`: classifies the sink as a CHAR
  `java/io/Writer` (`Some(true)`) vs a byte `java/io/OutputStream`
  (`Some(false)`) vs neither (`None`), using `class_id_of_object` +
  `class_id_by_name("java/io/Writer")` / `("java/io/OutputStream")` +
  `is_subclass`.
- `write_string_to_writer(ctx, out, text) -> bool`: writes via the real JDK
  `Writer.write(Ljava/lang/String;)V`. In real-JDK mode no synthetic
  Writer/BufferedWriter natives are registered, so this dispatches to the
  JDK's own `BufferedWriter`/`OutputStreamWriter`/`StreamEncoder` bytecode —
  **no stub added**.

`route_write_through_out`: after resolving `out`, if it is a `Writer`, write
chars via `write(String)` (text reconstructed from the UTF-8 bytes with
`String::from_utf8_lossy`); otherwise keep the existing `write([BII)V` byte
path. Sinks that are neither (`None`) keep prior byte behaviour.

`native_printwriter_printf`: compute `backing_is_writer` up front; never take
the `write([BII)V` byte fallback when the backing sink is a Writer.

## Notes
- GC safety: `sink_is_writer` performs only read-only class lookups (no
  safepoint), so the freshly re-resolved `out`/`arr` stay valid. The
  Writer path's `create_string` is the only allocation, and it occurs after
  `arr` is no longer used — matching the existing pattern in
  `native_printwriter_printf`.
- Helper APIs used (all read-only, from `native-api/src/registry.rs`):
  `class_id_of_object`, `class_id_by_name`, `is_subclass`, `create_string`,
  `invoke_virtual`.
