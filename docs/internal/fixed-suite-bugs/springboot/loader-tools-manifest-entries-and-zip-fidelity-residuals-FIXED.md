# `spring-boot-loader-tools` manifest and zip fidelity residuals

**Status: FIXED 2026-07-18.**

Three independent fidelity defects were closed:

- `JarFile.getManifest()` now retains and parses per-entry sections, including
  signed `*-Digest` attributes, rather than only the main attributes.
- IEEE `CRC32` uses its public JDK state representation. Its unsafe JIT
  intrinsic is disabled, while the audited native update bridge supplies the
  correct byte-for-byte calculation for the loader write path.
- `ByteArrayInputStream.read(byte[], int, int)` now preserves the JDK's
  immediate EOF behavior for a negative-count stream created by the
  three-argument constructor.

The related `Attributes` natives were also corrected to retain a null value.
HotSpot writes such a manifest value as the literal text `null`; CratonVM had
silently dropped the entry. This restores `Spring-Boot-Version` and the
one-time-repackage detection without fabricating package metadata.

## Validation

The dedicated release binary
`cratonvm-loader-tools-manifest-zip-20260718-019f7486.exe` passed the
affected Spring Boot loader-tools classes in both execution modes:

| Test class | JIT | `--nojit` |
| --- | ---: | ---: |
| `FileUtilsTests` | 6 passed | 6 passed |
| `ZipHeaderPeekInputStreamTests` | 9 passed | 9 passed |
| `RepackagerTests` | 52 passed | 52 passed |
| `ImagePackagerTests` | 37 passed | 37 passed |

The standalone manifest probe now emits `Spring-Boot-Version: null`, matching
HotSpot. Focused native-I/O, manifest-parser, CRC32, and forced-native bridge
Rust regressions also passed.
