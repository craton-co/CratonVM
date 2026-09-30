# Proposal: a per-VM post-collection native cleanup hook with a context

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 28
> of 54).** Not built (`native-io` `io_orphan_fds` / `close_orphan_fds` still
> per family). **Gate:** a dead `DatagramChannel`'s socket closed within one
> collection without another DatagramChannel call. **Size:** M.

**Status: PROPOSAL** (filed 2026-09-24, gc-common w16-c, on `69711aa8f`).
Not a defect page.

## The gap

The weak-row sweeps (`native-io/src/lib.rs::gc_sweep_io_side_tables`, the
Locale / TLS / logging / lock-key / httpserver sweeps) run in a collection
epilogue with no `NativeContext`. They can drop a Rust row, and they can
drop a Rust-owned resource such as a `WatchService` watcher. They cannot
release a resource that lives in a per-VM table reachable only through a
context. That covers:

- the VM's `FileDescriptorTable`, which holds a dead `DatagramChannel`'s UDP
  socket or a dead synthetic `RandomAccessFile`'s file;
- JNI global roots, which the `dc_socket_cache` fallback holds;
- anything that needs a Java upcall.

w16-c therefore parks such fds on `io_orphan_fds` and closes them from the
next native of the same family that holds a context: `native_dc_open` /
`native_dc_close` and the synthetic-RAF constructors / `close`. The leak is
bounded, but not prompt. A VM that stops opening channels keeps its parked
sockets until teardown. The open handoff for `ZipFile`
(`common-w16c-zip-archive-identity-tables-keyed-by-bare-hash`) will have
the same shape.

HotSpot does this with the `Cleaner` thread. A phantom-reachable
`DatagramChannelImpl`, `FileCleanable` or `ZipFile$CleanableResource` runs
its cleanup on a Java thread, with full VM access, shortly after the
collection that found it.

## Proposal

A small per-VM "post-collection native work" queue:

1. The epilogue sweeps push closures, or typed work items such as
   `CloseFd(vm, fd)` and `RemoveGlobalRoot(vm, handle)`, onto a per-VM queue
   instead of a per-family list.
2. The VM drains the queue with a real `NativeContext` at the first safe
   point after the collection. That is the reference-handler /
   finalizer-thread wake-up that `process_references_after_gc` already
   triggers, or the collecting thread's own return to the interpreter. It
   is never inside the stop-the-world pause.
3. native-io's `io_orphan_fds` / `close_orphan_fds` then become one producer
   of that queue, and `close_orphan_fds` calls disappear from the natives.

## Costs and risks

- One more per-VM structure, which must be drained at teardown like every
  other per-VM table. It is not a process global.
- The drain must not run with any collector or table lock held, and a
  panicking cleanup must be contained, as `close_dropped_watch_services`
  does with `catch_unwind`.
- It touches `vm/` (the drain site), so it needs a VM-owning lane.

## What would retire it

A drain with a context exists, the orphan-fd lists in native-io feed it,
and a dead `DatagramChannel`'s socket is closed within one collection cycle
without another DatagramChannel call.
