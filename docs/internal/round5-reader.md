# Round 5 — reader crate audit (round-4 verification + carry-over)

Round-4 wave-2 introduced `Arc<[u8]>` payloads for `CodeAttribute.code`, `StackMapTable.entries`, and `Unknown.data`, claiming "zero-copy" sharing with the class file buffer. Audit shows the claim is broken: `Arc::from(&[u8])` is not a sharing primitive. Several carry-over items from round-4 (#4, #5, #6, #7, #8) remain unfixed.

---

## [CRIT] 1. `Arc::from(&source[range])` allocates fresh — the wave-2 zero-copy fix is a no-op
**Status:** FIXED (round-5). `reader/src/byte_view.rs` introduces `ByteView { source: Arc<[u8]>, start, end }`. `CodeAttribute.code`, `StackMapTable.entries`, and `Unknown.data` now hold `ByteView`; the three decoder sites use `ByteView::new(Arc::clone(source), range)` — single atomic refcount bump, no memcpy. Downstream consumers that require an owned `Arc<[u8]>` (`VtableMethodSnapshot.code`) call `ByteView::to_arc()`, which pays the same alloc+memcpy cost once at vtable installation rather than on every parse. See round-5 fix below.

**File:** `reader/src/attribute.rs:731, 999, 1099` (now lines 741, 1015, 1117)

```rust
let code: Arc<[u8]> = Arc::from(&source[code_start..code_start + code_length]);
```

`impl From<&[T]> for Arc<[T]>` invokes `Arc::<[T]>::from(slice)` which allocates a brand-new `ArcInner<[T]>` whose data block is a memcpy of the slice (std's `arcinner_layout_for_value_layout` + `ptr::copy_nonoverlapping`). The parent `Arc<[u8]>` is *not* shared. Net effect vs. round-3 `to_vec()`: same number of allocations, same memcpy, plus 16 bytes of `ArcInner` header overhead per payload. The doc comments at lines 53, 600, 1070 are factually wrong; the bootstrap saving claimed in `round4-reader.md` is not realized.

**Fix:** True sharing requires `(Arc<[u8]>, Range<usize>)` payload (a "view"), or a third-party type like `bytes::Bytes`. Cheapest non-API-breaking option: store `code: SharedBytes` where `SharedBytes { src: Arc<[u8]>, range: Range<usize> }` and `impl Deref<Target=[u8]>`. Same for `StackMapTable.entries` and `Unknown.data`. Update all three sites + doc comments.

---

## [HIGH] 2. StackMap absolute_offset can panic on overflow (round-4 #6, unfixed)
**File:** `reader/src/stack_map.rs:152-155`

```rust
let absolute = match prev { None => delta, Some(p) => p + delta + 1 };
```

`p: u16 + delta: u16 + 1u16` panics in debug, wraps in release for frames near the end of a 64 KB method. Carry-over from round 4; not addressed.

**Fix:** Compute in `u32`, return `Vec<u32>`, and reject `absolute > 0xFFFF` with `InvalidClassData`.

---

## [HIGH] 3. `ClassFileBuffer` `pos + count` still unchecked (round-4 #7, unfixed)
**File:** `reader/src/buffer.rs:32, 46, 58, 74, 94, 102`

Every `read_uN` / `read_bytes` / `skip` computes `pos + count` and lets `slice::get(pos..pos+count)` reject overflow. On 32-bit `usize` this wraps before `get` is called and returns the wrong slice.

**Fix:** `pos.checked_add(count).and_then(|end| self.data.get(pos..end)).ok_or(...)` in all six call sites. Macro or inline helper to avoid repetition.

---

## [HIGH] 4. `decode_target_info` 0x40/0x41 still re-serializes table_length (round-4 #5, unfixed)
**File:** `reader/src/attribute.rs:1235-1244`

```rust
0x40 | 0x41 => { let table_length = buf.read_u16()?; let byte_count = 6 * table_length as usize;
    let mut data = Vec::with_capacity(2 + byte_count);
    data.push((table_length >> 8) as u8); data.push(table_length as u8);
    let raw = buf.read_bytes(byte_count)?; data.extend_from_slice(raw); Ok(data) }
```

Return type is still `Vec<u8>`; the be u16 prefix is hand-spliced. Per-method-with-localvar-annotations this is one alloc + memcpy.

**Fix:** Change `decode_target_info` return to `Arc<[u8]>` carved from the same `source` Arc threaded through `decode_attribute_body` (once finding #1 is fixed), or parse `target_info` into a structured enum so the byte form vanishes.

---

## [MED] 5. LineNumberTable / LocalVariable[Type]Table / Exceptions / InnerClasses per-u16 loops (round-4 #4, unfixed)
**File:** `reader/src/attribute.rs:685-707, 933-945, 947-959, 676-683, 696-707`; ExceptionTable `1101-1108`

Per-entry `read_u16()` × N fields × M entries, each with bounds check. Bulk slice + `u16::from_be_bytes` is 3-4x faster and lets the compiler vectorize.

**Fix:** `let raw = buf.read_bytes(entry_size * table_length as usize)?;` then chunk-parse with `for chunk in raw.chunks_exact(entry_size) { ... }`.

---

## [MED] 6. `decode_attribute_body` linear `match name` (round-4 #8, unfixed)
**File:** `reader/src/attribute.rs:656-1005`

~30-arm string match per decoded attribute. With the round-4 interning, every attribute name is an `Arc<str>` from `intern_arc`; pointer equality would work.

**Fix:** Cache a `OnceLock<HashMap<*const u8, AttributeKind>>` keyed by `Arc::as_ptr(name)`, populated lazily from `intern_arc(...).as_ptr()` for each known name. Fallback to string match for unknown.

---

## [MED] 7. `decode_attribute` (eager path) does redundant `Arc::from(bytes)` then immediately copies again
**File:** `reader/src/attribute.rs:592-593`

```rust
let source: Arc<[u8]> = Arc::from(bytes);
decode_attribute_with_source(name, &source, 0..source.len(), cp)
```

One `Arc::from(bytes)` copy here, then for `Code`/`StackMap`/`Unknown` a *second* `Arc::from(&source[..])` copy inside the body decoder (finding #1). Two memcpys for one decode.

**Fix:** Once finding #1 lands with view-style payloads, this path becomes single-copy. Until then, document the double-copy explicitly so callers know `decode_attribute` is not for hot paths.

---

## [LOW] 8. `validate_count` against `u16::MAX` is a no-op
**File:** `reader/src/class_reader.rs:29-33, 39-48`

`MAX_CP_SIZE = MAX_FIELD_COUNT = ... = u16::MAX`. `validate_count` reads a `u16` then checks `count > u16::MAX` — always false. The function is dead code.

**Fix:** Either lower the bounds to spec'd realistic limits (HotSpot uses 0xFFFF for CP but tighter for nested counts), or remove the checks and the constants. Current state implies safety it does not provide.

---

## Sanity notes (verified fine)

- `ConstantPool::get_class_name_arc` correctly returns `None` for non-`ClassReference` entries (`constant_pool.rs:155-162`).
- `Attribute::SourceFile(Arc<str>)` / `Signature(Arc<str>)` consumers use deref correctly (`class_file.rs:38-43`, `class_manager.rs:1714, 2035`).
- `ClassFile.{this_class, super_class, interfaces}: Arc<str>` consumers updated; only `.to_string()` falls back to `String` where ownership is genuinely required (`class_manager.rs:1714, 2035`).
- JFR's `StringPool` (`jfr/src/dump.rs:185`) is `Arc<[u8]>`-keyed for binary JFR-format strings; not interchangeable with `cratonvm_types::intern_arc` (`Arc<str>`, UTF-8). No unification opportunity.
- Magic/version/length checks at `class_reader.rs:69-80` use u32/u16 reads, not per-byte.
