# Brotli decompression/compression classes fail due to missing native `brotli.dll` on Windows

| | |
|---|---|
| **Status** | Confirmed NOT a CratonVM bug. Missing native dependency in Windows harness environment. |
| **Scope** | 4 classes: `BrotliDecompressorTest`, `BrotliIntegrationTest`, `HttpContentCompressorTest` (1 test), `HttpContentDecoderTest` (2 tests). |
| **Discovered** | 2026-09-23, full 739-class suite run on Windows host (`run-20260923-192409-passed`). |

## Symptom

Tests attempting to compress or decompress using Brotli (`com.aayushatharva.brotli4j`) fail with unsatisfied link errors:

```
java.lang.UnsatisfiedLinkError: Failed to load Brotli native library
    at com.aayushatharva.brotli4j.Brotli4jLoader.ensureAvailability(Brotli4jLoader.java:108)
    at io.netty.handler.codec.compression.Brotli.ensureAvailability(Brotli.java:71)
    at io.netty.handler.codec.http.HttpContentDecoderTest.testResponseBrotliDecompression(HttpContentDecoderTest.java:214)
Caused by: java.lang.UnsatisfiedLinkError: no C:\Users\Victor\AppData\Local\Temp\com_aayushatharva_brotli4j_...\brotli.dll in java.library.path
    at com.aayushatharva.brotli4j.Brotli4jLoader.<clinit>(Brotli4jLoader.java:81)
    at io.netty.handler.codec.compression.Brotli.<clinit>(Brotli.java:46)
```

Subsequent tests in the same VM then fail with:
```
java.lang.NoClassDefFoundError: Could not initialize class io.netty.handler.codec.compression.BrotliOptions
```

## Why this is not a CratonVM defect

`brotli4j` relies on unpacking and dynamically loading a platform-specific native binary (`brotli.dll` on Windows x86_64). When the Windows native binary is not present on `java.library.path` or cannot be extracted by the loader, Brotli compression is unavailable.

Tests such as `BrotliDecompressorTest` and `BrotliIntegrationTest` unconditionally invoke Brotli without guarding with `Assumptions.assumeTrue(Brotli.isAvailable())` (unlike `HttpContentDecompressorTest.testBrotliDecodingHonorsMaxAllocationAsOutputCap`, which properly checks the assumption and skips).

In `HttpContentCompressorTest` and `HttpContentDecoderTest`, all non-Brotli test cases pass cleanly (23 ok in compressor, 22 ok in decoder); only the specific test methods testing Brotli encoding/decoding fail.

## Disposition

Environment / harness dependency issue. To resolve, either include `brotli4j-native-windows-x86_64.jar` on the test classpath or skip Brotli tests on hosts where native Brotli is not installed.
