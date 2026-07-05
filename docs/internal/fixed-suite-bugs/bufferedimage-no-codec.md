# http.converter.BufferedImageHttpMessageConverterTests — no ImageIO/JPEG/PNG codec

Status: fixed

Date observed: 2026-07-04

Date fixed: 2026-07-05

## Fixed

CratonVM now routes the `BufferedImage` side-table methods and the public
`javax.imageio.ImageIO` PNG/JPEG read/write overloads through registered
natives in real-JDK mode. The fix adds PNG/JPEG encode/decode helpers to
`native-awt/src/image.rs`, stream/file `ImageIO.read` and `ImageIO.write`
bridges in `native-awt/src/natives.rs`, and force-native dispatch entries in
`vm/src/runtime/interpreter.rs`.

The real-JDK `BufferedImage` class has no synthetic `imageId` field, so the
native AWT bridge now also records `BufferedImage` object identity hash to
native image id. That preserves the existing synthetic-field path while making
real-JDK `new BufferedImage(...).setRGB(...)`, `ImageIO.write(...)`, and
`ImageIO.read(...)` share the same ARGB backing store.

Validation:

- `cargo check -p cratonvm-native-awt -p cratonvm-vm`
- `cargo test -p cratonvm-native-awt encode_decode -- --nocapture`
- `cargo test -p cratonvm-vm --test t7_desktop_conformance imageio -- --nocapture`
- One-off Java probe through a uniquely named binary
  `cratonvm-bufferedimage-codecs-20260705-001.exe`: PNG stream write/read,
  JPEG stream write/read, and PNG file write/read all passed.

## Summary

`org.springframework.http.converter.BufferedImageHttpMessageConverterTests`
ABENDs / fails on CratonVM (jit-real) while passing on HotSpot. Root cause is
architectural, not a small native bug: CratonVM has **no `javax.imageio`
codec of any kind** (`grep -rli "imageio"` across `native-builtins/`,
`native-io/`, `native-collections/`, `vm/`, and `native-awt/` returns zero
hits; no `image`/`png`/`jpeg-decoder`/`zune-*` crate is vendored in any
`Cargo.toml`).

`native-awt/src/image.rs` + `native-awt/src/natives.rs::register_image_natives`
implement only a bare ARGB-raster `BufferedImageData` side-table (keyed by an
`imageId` field stamped onto the Java object) for AWT/Swing painting —
`getRGB`/`setRGB`/`getWidth`/`getHeight`/`createGraphics`. The real JDK
`raster`/`colorModel` fields on `BufferedImage` are never populated, and
`ColorModel`/`Raster`/`SampleModel`/`sun/awt/image/*Raster` classes only get a
no-op `initIDs()` registered (comment: "CratonVM resolves fields by name, so
no IDs need caching" — i.e. deliberately incomplete).

`ImageIO.read(logo.jpg)` and `ImageIO.write(..., "png", ...)` run as ordinary
(uninstrumented) real JDK bytecode, which needs a registered
`ImageReaderSpi`/`ImageWriterSpi` backed by an actual JPEG/PNG codec —
CratonVM has neither. There's no dedicated stub either, so the failure mode
depends on what the real bytecode hits first: a missing native
(`NoClassDefFoundError: javax/imageio/ImageIO` was observed on this Linux
build, where `native-awt` is compiled out entirely — see Build note below),
or — if `native-awt`'s raster stub IS present — a crash/exception deep in the
real JDK PNG writer SPI when it dereferences the stub's null/uninitialized
`raster`/`colorModel`.

Already flagged as an admitted gap for an unrelated benchmark investigation:
`docs/internal/gaps/dacapo-luindex-sunflow-fop-investigation.md:57-70` (2026-06-01)
— "AWT/Java2D/ImageIO native surface... CratonVM doesn't implement... out of
proportion to one benchmark."

## Build note (Azure Linux host)

On the Azure Linux probe host (`/opt/cratonvm`, `/data/cratonvm`, and worktrees
under `/home/victor/wt-*`), `vm-cli/Cargo.toml` is LOCALLY patched (not
committed) to build `cratonvm-vm` with `default-features = false` — this
disables the `native-awt` feature entirely to dodge x11rb-0.13 API bitrot in
that crate. Under that patch, `BufferedImageHttpMessageConverterTests` fails
even earlier/differently (`UnsatisfiedLinkError: java/awt/Toolkit.initIDs()V`,
`NoClassDefFoundError: javax/imageio/ImageIO`) than it would with `native-awt`
compiled in (where the raster-stub-vs-real-codec mismatch above would apply
instead). Don't be surprised the failure signature differs between a
Linux build (awt disabled) and the original Windows+JDK25 report (awt
enabled, `native-awt`'s incomplete raster stub reached instead) — both paths
end up unfixed for the same underlying reason: no image codec exists.

## Repro

```bash
cd /opt/cratonvm/apps/spring-suite-runner   # or any dir with Woodstox etc. on classpath
KRUN_STACK=1 <cratonvm-binary> --java-home <jdk21-or-25> \
  -cp "$SPRINGWEB_TESTCP" KRun org.springframework.http.converter.BufferedImageHttpMessageConverterTests
```

## Original fix scope

The original note assumed a real fix required full
`Raster`/`ColorModel`/`SampleModel`/`DataBuffer` native wiring. The implemented
fix instead bypasses the real JDK ImageIO SPI for the supported PNG/JPEG
surface and encodes/decodes directly from CratonVM's authoritative ARGB
side-table. Full raster/color-model object parity remains outside this fixed
codec bug, but is no longer required for the Spring
`BufferedImageHttpMessageConverterTests` ImageIO path.
