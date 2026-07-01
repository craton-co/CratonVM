# Fixed: Brooks read-barrier vs CompactHeader forwarding slot

**Status:** FIXED. The live stop-the-world collector uses the legacy
`ObjectHeader.forwarding_ptr` field at offset 24, and the tier1 regression now
installs forwarding through that same field.

**Regression:**

```powershell
cargo test -p cratonvm-vm --test tier1_tests t1_brooks_barrier_follows_forwarding_pointer
```

Result on 2026-07-01: passed.

## Resolution

The original issue was a disagreement between the regression test and the read
barrier:

- The read barrier `gc/src/vm_heap.rs::load_and_forward` reads the full
  32-byte `ObjectHeader` and follows `ObjectHeader.forwarding_ptr`.
- The live generational collector `gc/src/gen_heap.rs::forward_object_impl`
  records relocation by writing `ObjectHeader.forwarding_ptr`.
- The test previously used `CompactHeader`, which encodes forwarding in the
  first 8 bytes and does not match the current heap object layout.

The test now writes the legacy forwarding field directly, so it exercises the
same path the collector and read barrier use. No `load_and_forward` runtime
change was required.
