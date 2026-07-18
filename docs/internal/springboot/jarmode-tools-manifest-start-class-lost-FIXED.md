# jarmode-tools manifest copy and launcher attributes — fixed

**Status: FIXED 2026-07-18**

## Root cause

The native `Manifest(Manifest)` bridge assigned the source manifest's main
`Attributes` and entries map directly to the new object. Spring Boot removes
launcher-only keys from the copied manifest, then reads `Start-Class` from the
original; aliasing made that removal mutate the original and produced the
spurious mandatory-attribute failure.

The same validation path exposed two directly related residuals:

- `JarFile.getManifest()` had a separate, physical-line parser that discarded
  continuation lines, truncating folded `Class-Path` values in generated
  launcher manifests.
- `Class.getResourceAsStream("")` resolves to the declaring class's package
  directory. The byte lookup correctly does not open directories, but the
  native then returned `null` instead of the non-null empty stream HotSpot
  exposes. This blocked the extraction safety test before it could reach its
  assertion.

## Fix

- Construct independent `Attributes` and `LinkedHashMap` instances in
  `Manifest(Manifest)`, preserving map semantics and iteration order.
- Route `JarFile.getManifest()` through the shared manifest parser, which
  joins RFC continuation lines before populating attributes.
- When a class resource has a URL but no file bytes because it is a directory,
  return an empty `ByteArrayInputStream` rather than `null`.

## Regression coverage and validation

`t10_manifest_parser_preserves_folded_main_attribute` protects the shared
parser's continuation behavior. The source-matched Spring Boot fixture was
run with the unique binary
`cratonvm-sb-jarmode-manifest-final-debug-20260718.exe` against Eclipse
Adoptium JDK 25.0.3.9:

| Class | Tests | JIT | `--nojit` |
|---|---:|:---:|:---:|
| `IndexedJarStructureTests` | 6 | PASS | PASS |
| `ExtractCommandTests` | 22 | PASS | PASS |

The runs verify the original `Start-Class` behavior, folded launcher
`Class-Path`, manifest entry generation, file-time preservation, and the
package-directory resource residual.
