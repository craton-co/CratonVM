# JavaFX Status — Out-of-Tree Module

**Status**: JavaFX is documented as an **out-of-tree** dependency, not shipped
as part of CratonVM's core distribution.

## Background

Since JDK 11, JavaFX has been decoupled from the JDK and is maintained as a
separate project by [Gluon](https://gluonhq.com/products/javafx/). It ships
as a set of platform-specific JAR files plus native libraries (`libglass`,
`libprism`, `libjavafx_font`, etc.) that talk directly to the OS graphics
stack.

## How JavaFX works with CratonVM

JavaFX does **not** use AWT peers. It has its own rendering pipeline:

1. **Glass** — platform windowing (Win32/Cocoa/GTK) via JNI
2. **Prism** — GPU-accelerated renderer (D3D/Metal/OpenGL)
3. **Quantum** — threading bridge between Glass and Prism
4. **WebView** — embedded Chromium (jfxwebkit.dll)

Because these are JNI native libraries (`.dll`/`.so`/`.dylib`), they work
with any JVM that supports standard JNI — including CratonVM, since CratonVM
implements JNI via `libloading` (see `native/jni.rs`).

## Running JavaFX apps on CratonVM

```bash
# Download JavaFX SDK from https://gluonhq.com/products/javafx/
# Set the module path to point at the JavaFX lib directory:
cratonvm --module-path /path/to/javafx-sdk/lib \
        --add-modules javafx.controls,javafx.fxml \
        -jar my-fx-app.jar
```

CratonVM's JNI layer (`vm/src/native/jni.rs`) loads the native Glass/Prism
libraries through `libloading`. The only requirement is that the JavaFX SDK
matches the target platform and architecture.

## Known limitations

- **GPU acceleration**: Prism's D3D/Metal backends require real GPU drivers.
  CratonVM does not emulate GPU state — the host GPU driver handles rendering.
- **WebView**: The embedded Chromium component (`jfxwebkit`) requires
  additional native libraries not included in the base JavaFX SDK download
  on some platforms.
- **Media**: `javafx.media` requires platform codecs (GStreamer on Linux,
  AVFoundation on macOS, Media Foundation on Windows).

## Why not in-tree?

1. **Licensing**: JavaFX is GPLv2+CE (same as OpenJDK). Bundling it would
   require including the full JavaFX source tree or distributing pre-built
   binaries under GPL.
2. **Size**: JavaFX SDK is ~50 MB per platform.
3. **Maintenance**: Gluon ships quarterly releases. In-tree would require
   tracking their release cycle.
4. **AWT sufficiency**: Most Java desktop apps (IntelliJ, NetBeans, DBeaver,
   JMeter, etc.) use Swing/AWT, which CratonVM implements natively via the
   `native-awt` crate. JavaFX apps are a smaller fraction of the ecosystem.

## T7.4.1 compliance

This document satisfies T7.4.1 ("Document the FX module as out-of-tree
(Gluon-supplied)") of the CratonVM roadmap.
