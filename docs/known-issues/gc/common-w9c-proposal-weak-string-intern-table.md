# Proposal: a weak String intern table, as HotSpot's `StringTable`

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 5
> of 54).** Not built (`vm/src/memory/roots.rs` still roots the pool as
> section 5; nothing removes an entry). HotSpot-parity leak with no
> workaround, plus one observable difference: `s.intern() == s` is `false` for
> a fresh `s` here and `true` on HotSpot. **Gate:** the page's test (intern a
> dynamic String, drop it, collect, entry gone) and a LoaderUnloadProbe-style
> row showing an unloaded class's literals reclaimed, on all three collectors;
> `--compatible` battery output unchanged. **Size:** M.

**Status: PROPOSAL** (filed 2026-09-24, gc-common round wave 9, lane C9).
All three collectors. Not a work item until triaged.

## What is there

`SharedVm.mem.string_pool` (`vm/src/vm/realms/heap_realm.rs`,
`FxHashMap<String, ObjectRef>`) is scanned as a STRONG root
(`vm/src/memory/roots.rs`, section 5, "Interned string pool") and remapped
in `vm/src/memory/gc.rs`. Nothing ever removes an entry. Everything that
goes through a pooled constructor stays live for the life of the VM:

- every `ldc` literal of every class, including classes that were unloaded;
- every `String.intern()` result
  (`native-builtins/src/lang_string.rs`, the `intern` native: `intern_arc`
  plus `ctx.create_string`). The `intern_arc` step also keeps the text's
  bytes in a process-wide `Arc<str>` pool, so the Rust-side copy outlives
  even the VM;
- anything a VM-internal path creates through `create_java_string` /
  `try_create_java_string` / `NativeContext::create_string` (pooled by
  default) instead of the uninterned forms.

HotSpot's `StringTable` holds its entries WEAKLY (JDK 7+, `WeakHandle`
entries cleaned by the GC's concurrent/parallel string-table cleaning). An
interned String nothing else references is collected. A literal stays alive
because the class's resolved-references array holds it, and dies with an
unloaded class.

## Why it matters here

- Programs that `intern()` request-derived keys (parsers, XML/JSON
  libraries, `Class.getName().intern()` idioms, older app servers) grow the
  old generation without bound on CratonVM, while HotSpot reclaims them.
- Wave 9 (C9) removed one such leak at the source: VM-raised exception
  detail messages were pooled (`exceptions.rs`,
  `create_exception_object_for_class_inner`), so every distinct
  `Index N out of bounds for length M` stayed live. Other pooled
  dynamic producers remain (for example the thread-plumbing name String in
  `vm/src/vm/vm_exec.rs`, `thread_plumbing_string`). Each is a leak only
  because the pool is strong.
- Class unloading cannot free a dead class's literals.

## Proposed direction

1. Make the pool weak: after marking, drop entries whose String is
   unmarked (a sweep hook beside the JNI weak-global sweep in
   `run_collection_pause`, which already runs per collection on all three
   backends), and remap the survivors as today.
2. Keep literals alive through their class, not the pool: record each
   resolved `ldc` String on the defining class (the per-entry
   `record_cp_constant_permanent` store already exists for condy /
   MethodHandle; `ldc` Strings are recorded only when
   `CRATONVM_JIT_NO_LDC_CONST_CACHE` is unset, so the recording would have
   to become unconditional for Strings), and scan those records as class
   roots, so a literal dies exactly when its class does.
3. Make `String.intern()` return the RECEIVER when the content is absent
   (JLS: "this String object is added to the pool and a reference to this
   String object is returned"). Today it allocates a new String, so
   `s.intern() == s` is `false` for a fresh `s` where HotSpot answers
   `true`. Drop the `intern_arc` side pool.

Risks: the JIT's `ldc` helpers cache String addresses; the young marker
must treat the table as weak on both young and full collections; the
`--compatible` identity of pooled VM-internal Strings changes only where a
String becomes unreachable, which is unobservable.

## Confirmation

```bash
rg -n "5: Interned string pool" vm/src/memory/roots.rs
rg -n "string_pool" vm/src --glob '!*tests*'
```

## What would retire it

A weak table with literals held by their classes, a test that interns a
dynamic String, drops it, collects, and sees the entry gone, and
`LoaderUnloadProbe`-style coverage that an unloaded class's literals are
reclaimed.
