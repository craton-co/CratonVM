# cratonvm-native-awt

AWT / Swing / Java2D native peer implementation for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Provides the native peers that back Java's desktop GUI stack: AWT,
Swing, and Java2D. Includes a platform-independent software renderer
that rasterizes into ARGB pixel buffers and a per-OS backend
(Win32 / X11 / Cocoa) responsible only for blitting the final buffer
and pumping the Event Dispatch Thread. Currently headless by default —
the renderer runs without a display for testability.

## Non-goals

- No JavaFX (separate stack, not backed by AWT peers).
- No hardware-accelerated 2D pipeline; software raster only.
- No bytecode for `javax.swing.*` Java classes — only native peers.

## Usage

```rust
use cratonvm_native_api::NativeMethodRegistry;

let mut reg = NativeMethodRegistry::new();
cratonvm_native_awt::register_awt_natives(&mut reg);
```

## Status

Pre-1.0 and headless-first. API stability is best-effort and tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

The registered natives are categorized as `Bridge` natives because they back
real JDK AWT/Swing/Java2D classes. They are not a full desktop backend:
`Frame.setVisible(true)` does not create an OS window yet, and the Win32/X11/Cocoa
backend modules remain scaffolded. In-process `Graphics2D` rendering, EDT
`invokeLater`/`invokeAndWait`, and EventQueue delivery for invocation, mouse,
key, window, and paint/update events are the supported readiness tier.

## Hardening Notes

- Pending EDT callbacks and peer event sources are held through VM global
  roots, then resolved at dispatch/event-synthesis time.
- Java-controlled image and renderer buffers have hard pixel caps and use
  fallible reservation paths before allocating backing storage.
- Geometry and coordinate arithmetic widens before combining Java `int`
  positions with dimensions, then clips or saturates at the native boundary.
- `copyArea` clips to the visible destination before allocating temporary
  storage, then processes large visible spans in bounded chunks.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
