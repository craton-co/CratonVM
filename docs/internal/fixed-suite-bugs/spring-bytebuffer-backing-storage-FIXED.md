# Spring ByteBuffer backing storage failures (FIXED)

Spring `ResourceRegionEncoderTests` and `MultipartHttpMessageWriterTests` could fail with:

`ByteBuffer missing backing array (field 0 returned Int(-1))`

The failing buffers used the real JDK `java.nio.Buffer` layout where slot 0 is `mark`, not a synthetic backing `byte[]`. Native ByteBuffer helpers now decode real heap buffers through `hb`/slot 5 plus `offset`, and direct buffers through `address` using the native-memory copy APIs. FileChannel, AsynchronousFileChannel, DatagramChannel receive, and common byte get/put paths share the same storage helper.

Verification:

- `cargo check -p cratonvm-native-io`
- `cargo test -p cratonvm-native-io buffer_bounds_tests -- --nocapture`
- `cargo test -p cratonvm-native-io`

The Windows-visible environment did not include `/data/data/spring-framework-shared`, `/data/data/spring-suite-runner-shared`, or `/data/data/jdk25-real`, so the Spring KRun class probes still need to be run on the shared Linux fixture host.
