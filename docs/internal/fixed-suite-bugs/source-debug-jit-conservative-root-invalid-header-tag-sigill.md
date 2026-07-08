# Source debug JIT Groovy repro hit SIGILL in conservative root validation

Status: FIXED 2026-07-08
Severity: Medium

## Symptom

The source-built debug CLI crashed while running the Groovy SAM scratch repro with JIT enabled:

```text
EXCEPTION_ILLEGAL_INSTRUCTION (SIGILL) at cratonvm_types::heap_types::element_byte_size
```

The same repro passed with the release binary and with source-built `--nojit`.

## Root Cause

The crash was not generated JIT code executing an illegal instruction. It was Rust debug code trapping
while conservative JIT-frame root scanning called `VmHeap::is_object_address`.

`GenerationalHeap::is_object_address` and `G1Collector::is_object_address` cast arbitrary in-arena bytes
to `ObjectHeader` and then matched typed `#[repr(u8)]` enum fields. A conservative stack/register word can
land on an interior value whose bytes look aligned and in-region, but whose `kind` or `element_type` byte
is not a valid enum discriminant. In debug builds, matching that invalid enum can lower to an illegal
instruction; this reproduced through `array_data_size(header.array_length, header.element_type)`.

## Fix

Added raw tag helpers and layout offsets in `types/src/heap_types.rs`:

- `object_kind_from_tag`
- `array_element_type_from_tag`
- `OBJECT_KIND_OFFSET`
- `ARRAY_ELEMENT_TYPE_OFFSET`

Both conservative object-address validators now read and validate the raw tag bytes before borrowing the
candidate address as `ObjectHeader`. Invalid tags are rejected as non-objects.

Regression tests corrupt only raw header bytes and verify both generational and G1 validators return
`None` rather than trapping.

## Verification

```powershell
cargo test -p cratonvm-gc is_object_address_rejects_invalid_raw_header_tags -- --nocapture
# 2 passed

cargo test -p cratonvm-types
# 306 unit tests + integration/doc tests passed

target\debug\cratonvm.exe --classpath 'scratch;C:\Users\Victor\.m2\repository\org\apache\groovy\groovy\4.0.32\groovy-4.0.32.jar' GroovySamCratonRepro
# OK
```

The source debug Groovy run took several minutes in the unoptimized build, but exited normally and no
longer hit SIGILL.
