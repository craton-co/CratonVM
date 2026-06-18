# Round 4 — reader crate perf/correctness review

8 findings sorted by impact.

---

## [HIGH] 1. `CodeAttribute::code` allocates a fresh `Vec<u8>` per method
**File:** `reader/src/attribute.rs:982`

```rust
let code = buf.read_bytes(code_length)?.to_vec();
```

Each decoded `Code` attribute clones the bytecode into a fresh heap allocation, even though the body is already inside an `Arc<[u8]>` pinned by the parent `LazyAttribute::Raw`. On java.base bootstrap (~5k classes × ~10 methods, avg ~80 bytes of bytecode) this is ~4 MB of avoidable malloc + memcpy on first interpret.

**Fix:** Change `CodeAttribute.code` from `Vec<u8>` to `Arc<[u8]>` and slice/clone the parent Arc. Plumb `source: Arc<[u8]>` + range through `decode_attribute` → `decode_code_body` (currently only `&[u8]` is passed). Same applies to `StackMapTable.entries` (line 649) and `Unknown.data` (line 906).

---

## [HIGH] 2. `SourceFile` / `Signature` decode allocates fresh `String` instead of cloning interned `Arc<str>`
**File:** `reader/src/attribute.rs:585-595` and `635-645`

```rust
let source_file = cp.get_utf8(source_file_index).ok_or(...)?.to_string();
Attribute::SourceFile(source_file)
```

CP already holds `Arc<str>` (round 3 interning). `.to_string()` defeats interning. Same pattern for `Unknown { name: name.to_string() }` (line 908) and `decode_attributes_vec` nested attribute name (line 936).

**Fix:** Change `Attribute::SourceFile(String)` → `Attribute::SourceFile(Arc<str>)` (and `Signature`, `Unknown::name`). Use `cp.get_utf8_arc(idx)` and `Arc::clone(&name)`.

---

## [HIGH] 3. `read_class` materializes `this_class`/`super_class`/`interfaces` as fresh `String`s
**File:** `reader/src/class_reader.rs:91-128` and `class_file.rs:14-16`

```rust
let this_class = constant_pool.get_class_name(...)?.to_string();
```

`ClassFile.this_class: String`, `super_class: Option<String>`, `interfaces: Vec<String>` are all derived from CP Utf8 entries that are already `Arc<str>`. ~15k avoidable allocations on bootstrap.

**Fix:** Change the three fields to `Arc<str>` / `Option<Arc<str>>` / `Vec<Arc<str>>`. Add `ConstantPool::get_class_name_arc()`. Also matches the type already used for `ClassFileField.name`/`ClassFileMethod.name`.

---

## [MED] 4. Per-u16 reads in dense tables — bulk slice + chunk parsing is ~3-4× faster
**File:** `reader/src/attribute.rs` — `LineNumberTable` (611-621), `LocalVariableTable` (849-862), `LocalVariableTypeTable` (863-876), `ExceptionTable` (985-994), `InnerClasses` (622-634)

Each entry decoded with N separate `read_u16()` calls. `LocalVariableTable` (5×u16 = 10 bytes/entry) with 100 locals does 500 bounds-checked single-u16 reads.

**Fix:** Do one `buf.read_bytes(entry_size * count)?` and parse u16 fields directly from the slice with `u16::from_be_bytes`. Compiler can vectorize the linear sweep.

---

## [MED] 5. `decode_target_info` re-serializes table_length into a Vec for type annotation kinds 0x40/0x41
**File:** `reader/src/attribute.rs:1117-1126`

```rust
0x40 | 0x41 => {
    let table_length = buf.read_u16()?;
    let byte_count = 6 * table_length as usize;
    let mut data = Vec::with_capacity(2 + byte_count);
    data.push((table_length >> 8) as u8);
    data.push(table_length as u8);
    let raw = buf.read_bytes(byte_count)?;
    data.extend_from_slice(raw);
    Ok(data)
}
```

**Fix:** Hold `target_info` as an `Arc<[u8]>` slice into the parent attribute, OR parse target_info eagerly into a structured enum.

---

## [MED] 6. Stack-map `absolute_offsets` can overflow u16 silently
**File:** `reader/src/stack_map.rs:152-154`

```rust
let absolute = match prev {
    None => delta,
    Some(p) => p + delta + 1,
};
```

For frames near the end of a 64KB method, `prev + delta + 1` can exceed `u16::MAX` and panic in debug or wrap in release.

**Fix:** Compute as `u32`: `Vec<u32>` return, `p as u32 + delta as u32 + 1`. Validate `absolute <= 0xFFFF`.

---

## [MED] 7. `ClassFileBuffer` arithmetic uses unchecked `pos + count`
**File:** `reader/src/buffer.rs:90-98, 100-107`

```rust
let bytes = self.data.get(pos..pos + count).ok_or(...)?;
```

`pos + count` can wrap on 32-bit targets. 64-bit is fine but the contract should be robust.

**Fix:** `pos.checked_add(count).and_then(|end| self.data.get(pos..end))` in `read_bytes` and `skip`.

---

## [LOW] 8. `decode_attribute_body` `match name` is a linear string compare per attribute
**File:** `reader/src/attribute.rs:583-911`

30-arm match on `name` does string equality per arm. Over ~50k attributes on bootstrap this is O(N × 30) string compares.

**Fix:** Pre-compute `&'static str → fn` lookup (or `phf` map), OR if attribute names live in `Arc<str>` from the CP, pointer-compare against a sentinel set.

---

## Sanity notes (already fine)

- Tableswitch / lookupswitch padding `next % 4` is correct.
- Wide prefix dispatch complete (0x15-0x19, 0x36-0x3a, 0x84, 0xa9) with rejection of invalid wide opcodes.
- Invokeinterface / invokedynamic reserved-byte zero-check present.
- Switch entry caps (16,384) and pre-allocation caps (1,024) in place.
- Lazy attribute Arc plumbing at `class_reader.rs:64, 419-473` good after rounds 1-3.
- No `unimplemented!()` / `todo!()` / `panic!()` in non-test code. All opcodes 0x00-0xC9 handled.
