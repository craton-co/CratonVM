# ZGC Architecture — Low-Latency Colored Pointers and Page Allocator

**Status:** Shipped (default GC collector). Concurrent marking is opt-in
(`CRATONVM_ZGC_CONC_START=60`), STW sweep is default, and compaction is fully supported.

## What it does today

ZGC is CratonVM's primary low-latency garbage collector (`gc/src/zgc.rs`, `gc/src/arena.rs`).
It is designed to achieve low pause times on heaps ranging from small embedded footprints
to terabytes, using colored pointer metadata and page-oriented allocation.

### 1. Pointer Coloring and Address Space

ZGC represents reference values as 64-bit colored pointers:
- **Bits 0..43 (44 bits):** Object offset within the virtual address space (up to 16 TB).
- **Bits 44..45 (2 bits):** Marking color flags (`Marked0`, `Marked1`). Alternating mark colors
  allow immediate cycle differentiation without clearing mark bits across the entire heap.
- **Bit 46 (1 bit):** `Remapped` flag. Indicates whether an object reference has been updated
  to its relocated forwarding address.
- **Bits 47..63:** Reserved / unmapped bits.

Pointer dereferences and field accesses use address masking to strip color bits when accessing
physical memory, or rely on virtual memory address alias mappings.

### 2. Page-Based Arena and Memory Organization

The heap is divided into distinct page classes based on allocation size:
- **Small Pages (2 MB):** Used for objects up to 256 KB. Small pages are fast-allocated from
  thread-local allocation buffers (TLABs).
- **Medium Pages (32 MB):** Used for objects greater than 256 KB and up to 4 MB.
- **Large Pages (Variable Size):** Sized to exact multiples of 2 MB to house a single large
  object (> 4 MB). Large pages are allocated directly from global page free lists and are not
  relocated (they are unmapped directly when unreachable).

### 3. Collection Lifecycle and Phase Execution

ZGC operates via five coordinated phases:

1. **Pause Mark Start (STW):**
   - Pauses all mutator threads briefly.
   - Arms root filters (`external_roots.rs`).
   - Scans thread stacks, JNI local/global handles, VM internal tables, and static class roots.
   - Switches the active mark color (e.g., from `Marked0` to `Marked1`).
2. **Concurrent Mark:**
   - Resumes mutators.
   - Background GC worker threads drain marking queues and traverse live object graphs.
   - Write/load barriers ensure any newly referenced objects are colored with the current mark bit.
3. **Pause Mark End (STW):**
   - Pauses mutators to complete final mark draining.
   - Executes reference processing (Soft, Weak, Phantom references, and Finalizers).
   - Identifies candidate pages for compaction and relocation based on fragmentation and liveness ratios.
4. **Pause Relocate Start (STW):**
   - Allocates forwarding tables for pages selected into the relocation set.
   - Evacuates roots to ensure thread stacks hold valid relocated references before resume.
5. **Concurrent Relocate:**
   - Resumes mutator threads.
   - Background workers copy surviving objects from relocation set pages into newly allocated pages.
   - Forwarding entries are recorded in the page's forwarding table.
   - When mutators load a reference pointing to an unevacuated page, the load barrier intercepts the access,
     evacuates or fetches the forwarded address, and atomically heals the pointer slot (`self-healing`).

### 4. Vtable and Class Metadata Management

- **Unified Vtable Storage:** Class vtables are managed with a single copy shared across the
  runtime rather than duplicated per instance, avoiding significant metadata bloat in large
  class hierarchies.
- **Class Unloading:** Metaspace and class definitions are swept during quiescent phases,
  reclaiming obsolete vtables, native method bindings, and constant pools.

## Flags and Runtime Configuration

| Flag | Default | Description |
|---|---|---|
| `CRATONVM_GC` | `zgc` | Selects ZGC as the active collector |
| `CRATONVM_ZGC_CONC_START` | unset | Heap occupancy percentage trigger for concurrent marking |
| `CRATONVM_ZGC_COMPACT` | `1` | Enables compaction of fragmented pages |
