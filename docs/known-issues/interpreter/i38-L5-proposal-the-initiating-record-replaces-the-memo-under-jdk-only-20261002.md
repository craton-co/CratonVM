# Proposal: under `--jdk-only`, the initiating RECORD replaces the capped memo as the loader's answer

**Status: proposal — filed 2026-10-02 by interpreter round i1 wave 38, lane
L5. Not implemented.**

## Where things stand after wave 38

Two per-VM tables now say what a user loader answers for a name it did not
define:

* `ClassRealm::initiating_resolution_cache` (the memo, since 2026-07): capped
  at 4096 rows per loader, FIFO-evicted, and written by four writers, two of
  which HotSpot has no counterpart for — a receiver call's owner unknown to the
  caller memoises the GLOBAL fallback's answer after the loader refused
  (`invoke.rs` `resolve_owner_unknown_to_caller`, wave 29), and `drive_loader_for_global_name` memoises the
  built-in class for a JDK-global name the loader refused (wave 28).
* `ClassRealm::initiating_records` (wave 38,
  `vm/src/runtime/resolve/initiating_records.rs`): never capped, written only
  where HotSpot writes a dictionary entry (a successful checked drive,
  `Class.forName` with the loader), read by `findLoadedClass`, the define
  backend and the racing-winner re-read.

Every loader-namespaced resolution (`resolve_class_loader_aware`'s `known`
step, `field_access.rs`, `dispatch_static.rs`, `class_resolved_without_loading`
for the JIT) answers from the memo first, so a fallback answer the memo
recorded is served as if the loader had given it, and an evicted row re-drives
the loader. Wave 38's loader-throw propagation already stops one of the two
non-HotSpot writers for loaders whose `loadClass` is bytecode (the resolution
now fails instead of falling back), but the rows remain for native-`loadClass`
loaders and JDK-global names.

## The proposal

Under `--jdk-only` only:

1. `lookup_loader_initiated` answers from the loader's own definitions, then
   the RECORD; the memo is not read. `--compatible` keeps the memo exactly.
2. The two non-HotSpot memo writers become no-ops under `--jdk-only`; their
   purpose (not re-asking a refusing loader on every site-cache miss) is
   served instead by recording the resolution FAILURE against the entry,
   which the class opcodes already do for a `LinkageError`.
3. `cache_loader_initiated` stays for `--compatible`; under `--jdk-only` the
   checked drive's `record_initiating_load` is the only writer.

## Why it is not done here

It changes what every loader-namespaced resolution reads, on the path Spring
Boot's `LaunchedClassLoader` classes take for every `new`. Measure first: a
`CRATONVM_DBG=access` count of `known` answers whose memo row has no record
behind it (the rows the proposal would stop serving), over the suite, the
jdk-only corpus, the Spring Boot fat jar and the Tomcat loader tests. A
non-zero count names exactly the resolutions that would change; each must be
a fallback answer HotSpot would not give.

## Cost

One map instead of another on the same path; the record table is not capped,
so its size is the number of distinct delegated names per live loader
(bounded by the classes the program references), dropped with the loader by
the unload sweep.
