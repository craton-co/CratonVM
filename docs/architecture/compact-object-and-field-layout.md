# Compact object and field layout

Status: implemented

## Header contract

Production objects use one 32-byte, 8-byte-aligned `ObjectHeader`:

| Offset | Width | Meaning |
| ---: | ---: | --- |
| 0 | 4 | class id |
| 4 | 1 | object kind |
| 5 | 1 | array element kind |
| 6 | 1 | GC age |
| 7 | 1 | GC flags |
| 8 | 4 | identity hash |
| 12 | 4 | array length or full object field count |
| 16 | 8 | forwarding pointer |
| 24 | 8 | mark word |

The JIT imports exported offset constants rather than duplicating numeric
offsets. The `shape` word deliberately preserves all 32 field-count bits;
legal large inherited layouts must not truncate.

## Field representation

Compact instance fields are tagless and naturally aligned. Boolean and byte
use one byte, char and short use two, int and float use four, and references,
longs, and doubles use eight. The body is rounded to an 8-byte boundary.
`Value` remains the interpreter and helper API representation, but it is not
stored in compact production object bodies.

Each class layout contains field offsets, storage kinds, and the reference
offset oop-map used by every collector. All compact reads and writes are
width-correct atomic operations. Reference stores use the GC barrier path when
required.

## Layout stability

Objects select metadata by `(class_id, field_count)`. Append-only
synthetic-class upgrades publish a new layout version without changing the
size or oop-map of already allocated objects. Legal JVM redefinition cannot
change the field schema while retaining the same field count; a same-key
registration therefore refreshes the entry for independent class-manager
lifetimes that reuse numeric class IDs.

Layout metadata remains live at least as long as instances can be live.
Class-loader unloading is responsible for reclaiming versions only after the
loader and all instances are proven unreachable.

## Compatibility

The compact flag is per object. Legacy 16-byte `Value`-cell objects remain
readable in the same process, including objects created before a complete
class layout was available. Arrays retain their existing packed element
representation and use the same 32-byte header.

## Validation

Regression tests pin every header offset, exercise a field count above 16
bits, round-trip every tagless storage kind, retain old layout versions across
append-only upgrades, and scan compact references in the supported
collectors. Runtime validation covers JIT and interpreter execution.
