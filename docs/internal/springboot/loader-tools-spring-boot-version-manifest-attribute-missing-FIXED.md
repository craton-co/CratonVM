# `Spring-Boot-Version` manifest attribute missing from packaged jars

**Status: FIXED 2026-07-18.**

`Packager` validly receives a null implementation version for this exploded
Gradle classpath. HotSpot retains `Attributes.putValue(name, null)` and writes
it as `name: null`; CratonVM's native `Attributes.putValue` and `put` instead
dropped null mappings. The absent manifest entry also made a second repackage
mistake the first output for an unpackaged jar.

Both native insertion paths now retain null values while pinning only
non-null references. This restores the normal `Map` contract, preserves
`Spring-Boot-Version`, and lets `isAlreadyPackaged` recognize the first pass.

## Validation

The dedicated release binary passed `RepackagerTests` (52/52) and
`ImagePackagerTests` (37/37), each in JIT and `--nojit` modes. A standalone
manifest probe also emits `Spring-Boot-Version: null`, matching HotSpot.
