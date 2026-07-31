# `SIGSEGV` under sustained heap pressure: the GC's corrupt-header diagnostic Debug-formatted the invalid enum it had just rejected

## Status
**FIXED** (2026-07-31, branch `fix/class-mirror-segv-20260731`). The crash is
gone and the condition behind it now does what it was always supposed to do —
warn, and let the walker re-sync.

## Symptom

A hard `SIGSEGV` (no Java exception, no usable report) after several minutes of
near-exhaustion GC, e.g. `--Xmx 512m` on H2's `org.h2.test.db.TestOutOfMemory`.
`addr` varied between occurrences (`0x0` and other bogus values), and the
faulting frame was always inside string formatting:

```
#0  next_code_point<core::slice::iter::Iter<u8>>   core/src/str/validations.rs:37
#2  write_str                                      tracing_subscriber .../escape.rs:17
#4  core::fmt::write
...
#33 warn_non_object_kind_in_object_arm             gc/src/gen_heap.rs:10706
#34 gen_object_total_size                          gc/src/gen_heap.rs:11199
#35 scan_object_for_old_refs                       gc/src/gen_heap.rs:8801
#36 old_gen_gc                                     gc/src/gen_heap.rs:8531
#37 major_gc                                       gc/src/gen_heap.rs:8413
#38 collect_garbage_inner                          gc/src/gen_heap.rs:5388
```

## Root cause

`gen_object_total_size` screens a header it cannot size and calls
`warn_non_object_kind_in_object_arm`. The call site's own comment already spelt
out the precondition:

> Testing `!= Object` rather than `== HumongousFiller` also covers **a header
> whose kind byte is not a valid `ObjectKind` discriminant at all** — notably a
> `GAP_FILLER_CLASS_ID` sentinel, whose 8-byte span puts the low byte of its
> length field (8/16/24/32) where `kind` lives.

And then the diagnostic did:

```rust
tracing::warn!("GC: header kind={:?} ...", header.kind, ...);
```

`ObjectKind` is `#[repr(u8)]` with discriminants 0..=2. The derived `Debug` for
such an enum resolves the variant *name* by discriminant; on an optimized build
that is an indexed lookup into a static name table. Formatting discriminant
`0xc6` (198) therefore reads a `&str` from ~198 entries past the end of a
3-entry table, and the formatter then walks that wild pointer — `SIGSEGV`
inside `core::fmt`. The function whose entire job is to *report* a corrupt
header was itself the thing that turned a recoverable detection into a process
kill.

The kind bytes actually observed in the wild (from the fixed build, which now
prints them):

```
GC: header kind=0x04 reached the legacy-object sizing arm (shape=0,   class_id=6021)
GC: header kind=0x04 reached the legacy-object sizing arm (shape=512, class_id=4)
GC: header kind=0x16 reached the legacy-object sizing arm (shape=0,   class_id=6004)
GC: header kind=0xc6 reached the legacy-object sizing arm (shape=512, class_id=4)
```

`0x04`, `0x16`, `0xc6` — none of them a valid discriminant.

**This hazard was already known and already fixed at exactly one site.** The
non-moving sweep's own corrupt-header path carries a paragraph explaining it
("format the RAW kind byte, not `header.kind` via Debug ... observed: JIT
miscompile of `JUnitCore.main` under real-JCA"). The lesson had not been carried
to the sibling diagnostics.

## Fix (`gc/src/gen_heap.rs`)

Every diagnostic on a corrupt-header path now formats the raw byte:

1. `warn_non_object_kind_in_object_arm` — `header.kind` → `header.kind as u8`,
   printed `0x{:02x}`. **This is the crash.**
2. `warn_corrupt_array_header` — same for `element_type`. `ArrayElementType`'s
   valid discriminants are 4..=11, so an out-of-range byte there is *more*
   likely than for `ObjectKind`, not less.
3. `get_header_diagnostics` — the `CRATONVM_DBG_STALE_OBJREF` path formatted the
   kind of a header reached through an *untrusted* forwarding pointer. (This one
   is what crashed the first gdb capture attempts, before the flag was dropped.)
4. The same diagnostic's old-gen holder scan.
5. The `[A2-FL]` free-list overlap clamp, reached only for an already-suspect
   over-sized header.
6. `gen_object_total_size`'s screen now compares the raw byte
   (`header.kind as u8 != ObjectKind::Object as u8`). The paragraph above it
   already conceded that "the optimiser is entitled to assume `kind` is in
   0..=2" — which is precisely what could let it fold away the screen for the
   out-of-range case the screen exists to catch.

## Reproduction (a 3-to-8 minute deterministic probe)

The original repro was the whole H2 class at 9–13 minutes, roughly one run in
two. Extracting the phase that was actually executing (per a
`--stack-dump-on-timeout` capture) gives a far better vehicle —
`MemFsInsertProbe`: register H2's in-memory filesystem and build a table of
10 000 ~1 MB strings through the MVStore insert path, three times.

```bash
cratonvm --java-home <jdk25> --Xmx 512m -c ".:<h2>/target/classes" MemFsInsertProbe 3
```

**Before: 3/3 SIGSEGV** (also 3/3 under `gdb -batch`, which is how the stack
above was captured). **After: 0/4**, and one of those runs hit the corrupt-header
condition five times, logged all five, re-synced and ran to completion — the
designed behaviour.

Keep the JIT **on**: every collection in the failing window logged
`[moving-young] fallback: reason=unregistered-jit-frame-on-stack`, i.e. the
non-moving sweep, which is the walker that meets these headers.

## Validation

* `cratonvm-gc --lib` 877 pass / 0 fail (including the new regression test);
  `cratonvm-vm --lib` 2326 pass / 0 fail.
* New test `corrupt_kind_byte_is_reported_as_a_raw_byte_not_debug_formatted`
  builds a header with `kind = 24` (the documented 24-byte-gap-filler shape),
  installs a capturing `tracing::Subscriber` — load-bearing, because
  `tracing::warn!` does not materialise its arguments unless something is
  listening, so without one the test passes on the broken code — and asserts
  both that the size screen returns 0 and that the message names `kind=0x18`.
  Negative control: restoring only the `{:?}` formatting makes it fail.
* Note what the negative control also showed: in a **debug** build `{:?}` on
  discriminant 24 printed `HumongousFiller` (the comparison chain falls through
  to the last arm) rather than crashing. The wild read is an **optimized-build**
  behaviour. So the unit test guards the *shape* of the regression; the crash
  itself only reproduces in release, which is why it took the H2 workload to
  surface it.

## Not fixed here (deliberately)

**Why a header with an invalid `kind` byte is reachable by the old-gen scan at
all** is a separate question and is unchanged by this commit. The walker's
designed response to one is "treat as corrupt, return 0, re-sync" — that
contract was already written down at the call site; it simply could not execute,
because reporting the condition killed the process first. The diagnostics this
commit repaired are now the instrument for investigating the upstream cause: the
`shape=512, class_id=4` entries above are a concrete starting point.

## Method note — a retracted attribution

The first version of this record named
`cratonvm_vm::vm::vm_object::resolve_class_mirror_slots`, and the file was
originally called `...class-mirror-sigsegv...`. That was wrong. The address fed
to `addr2line` had been computed from `/proc/self/maps` as
`pc - segment_start + segment_file_offset`, which is a **file offset**;
`addr2line` wants a **virtual address**. The executable `LOAD` header here has
`p_vaddr - p_offset = 0x1000`, so every probe was 4 KiB low and named the
preceding function. The tell was that `addr2line` returned a function name but
`cgu.0:?` for file:line — `.debug_line` *is* present in this profile, and a
correct address returns a real `file.rs:NNN`.

```
va = pc - segment_start + segment_file_offset + (p_vaddr - p_offset)
```

Under `lto = "fat"` + `codegen-units = 1` a single symbolized address is weak
evidence even when computed correctly. The gdb backtrace above is what actually
settled this; getting it needs
`handle SIGUSR1/SIGUSR2 nostop noprint pass` first, or gdb stops on one of the
VM's internal signals and the dump looks like a healthy idle process.
