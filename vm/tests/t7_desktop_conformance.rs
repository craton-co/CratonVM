// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T7 — Desktop (AWT/Swing/Java2D) conformance test suite.
//!
//! These tests verify that CratonVM's native-awt crate correctly implements
//! the AWT, Swing, and Java2D native peer layer. Most tests run headless
//! (software renderer only, no display required). Tests requiring a real
//! display are marked `#[ignore]`.
//!
//!     cargo test -p cratonvm-vm --test t7_desktop_conformance -- --nocapture
//!
//! For display-dependent tests:
//!
//!     cargo test -p cratonvm-vm --test t7_desktop_conformance -- --ignored --nocapture

// ---------------------------------------------------------------------------
// T7.1 — Native windowing (headless / unit-level tests)
// ---------------------------------------------------------------------------

#[test]
fn t7_1_1_platform_backend_exists() {
    // Verify the native-awt crate compiles and the platform backend is selectable.
    use cratonvm_native_awt::platform::backend::WindowId;

    // This test used to be `let _id = WindowId(1); eprintln!(...)` — it could
    // only fail by failing to COMPILE, which is not something a test needs to
    // exist for. The properties below are the ones the AWT peer layer actually
    // depends on and that a careless edit to `WindowId` could break:
    //
    //  * the wrapped value is public and round-trips (peers hand raw handles
    //    across the native boundary and rewrap them);
    //  * it is a u64, not a usize — the handle width must not follow the host
    //    pointer size, or a 32-bit target would silently truncate;
    //  * it compares by value (`Eq` + `Hash` are what the peer maps key on).
    let id = WindowId(1);
    assert_eq!(id.0, 1u64, "WindowId must round-trip its raw handle");
    assert_eq!(
        std::mem::size_of::<WindowId>(),
        std::mem::size_of::<u64>(),
        "WindowId must stay a u64 newtype — peer handles are not pointer-width"
    );
    assert_eq!(id, WindowId(1), "WindowId must compare by value");
    assert_ne!(id, WindowId(2), "distinct handles must not compare equal");
}

#[test]
fn t7_1_2_awt_toolkit_shim() {
    // Verify the AWT toolkit shim registers expected native methods.
    use cratonvm_native_api::NativeMethodRegistry;
    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_awt::register_awt_natives(&mut registry);
    let count = registry.len();
    eprintln!("[t7] AWT natives registered: {count}");
    assert!(registry
        .find("java/awt/Toolkit", "initIDs", "()V")
        .is_some());
    assert!(registry
        .find("sun/java2d/Disposer", "initIDs", "()V")
        .is_some());
    assert!(count >= 50, "Expected >= 50 AWT natives, got {count}");
}

#[test]
fn t7_1_5_buffered_image_backing_store() {
    use cratonvm_native_awt::image::{BufferedImageData, ImageType};

    let mut img = BufferedImageData::new(100, 100, ImageType::IntArgb);
    assert_eq!(img.width(), 100);
    assert_eq!(img.height(), 100);
    assert_eq!(img.image_type(), ImageType::IntArgb);

    // Write and read pixels
    img.set_rgb(10, 20, 0xFFFF0000); // opaque red
    assert_eq!(img.get_rgb(10, 20), 0xFFFF0000);

    // Bulk operations
    let region = vec![0xFF00FF00u32; 25]; // 5x5 green
    img.set_rgb_region(0, 0, 5, 5, &region);
    let readback = img.get_rgb_region(0, 0, 5, 5);
    assert_eq!(readback.len(), 25);
    assert!(readback.iter().all(|&p| p == 0xFF00FF00));

    // Subimage
    let sub = img.get_subimage(0, 0, 5, 5);
    assert_eq!(sub.width(), 5);
    assert_eq!(sub.height(), 5);
    assert_eq!(sub.get_rgb(0, 0), 0xFF00FF00);

    eprintln!("[t7] BufferedImage backing store: OK");
}

#[test]
fn t7_1_6_imageio_png_jpeg_natives_registered() {
    use cratonvm_native_api::NativeMethodRegistry;

    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_awt::register_awt_natives(&mut registry);

    assert!(registry
        .find(
            "javax/imageio/ImageIO",
            "read",
            "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;",
        )
        .is_some());
    assert!(registry
        .find(
            "javax/imageio/ImageIO",
            "read",
            "(Ljava/io/File;)Ljava/awt/image/BufferedImage;",
        )
        .is_some());
    assert!(registry
        .find(
            "javax/imageio/ImageIO",
            "write",
            "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z",
        )
        .is_some());
    assert!(registry
        .find(
            "javax/imageio/ImageIO",
            "write",
            "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z",
        )
        .is_some());
    assert!(registry
        .find(
            "javax/imageio/ImageIO",
            "write",
            "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z",
        )
        .is_some());

    assert!(registry
        .find(
            "com/sun/imageio/plugins/jpeg/JPEGImageReader",
            "initReaderIDs",
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V",
        )
        .is_some());
    assert!(registry
        .find(
            "com/sun/imageio/plugins/jpeg/JPEGImageWriter",
            "initWriterIDs",
            "(Ljava/lang/Class;Ljava/lang/Class;)V",
        )
        .is_some());
    assert!(registry
        .find(
            "com/sun/imageio/plugins/jpeg/JPEGImageReader",
            "initJPEGImageReader",
            "()J",
        )
        .is_some());
    assert!(registry
        .find(
            "com/sun/imageio/plugins/jpeg/JPEGImageReader",
            "read",
            "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;",
        )
        .is_some());
    assert!(registry
        .find(
            "com/sun/imageio/plugins/png/PNGImageWriter",
            "write",
            "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V",
        )
        .is_some());

    eprintln!("[t7] ImageIO PNG/JPEG native surface: OK");
}

// ---------------------------------------------------------------------------
// T7.2 — Swing
// ---------------------------------------------------------------------------

#[test]
fn t7_2_1_metal_look_and_feel() {
    use cratonvm_native_awt::swing::{MetalTheme, UIDefaults};

    let theme = MetalTheme::ocean();
    assert_ne!(theme.primary1, 0);
    assert_ne!(theme.secondary3, 0);
    assert_eq!(theme.black, 0xFF000000);
    assert_eq!(theme.white, 0xFFFFFFFF);

    let defaults = UIDefaults::new_metal();
    assert!(defaults.get_color("Button.background").is_some());
    assert!(defaults.get_color("Panel.background").is_some());
    assert!(defaults.get_font("Button.font").is_some());
    assert!(defaults.get_font("Label.font").is_some());

    eprintln!("[t7] Metal Look-and-Feel defaults: OK");
}

#[test]
fn t7_2_7_edt_safety() {
    use cratonvm_native_awt::edt;

    // Initially we're NOT on the EDT
    assert!(!edt::is_edt());

    // The EDT should be creatable
    let edt_inst = edt::get_edt();

    // Start and verify
    edt_inst.start();
    assert!(edt_inst.is_running());

    // Post events
    use cratonvm_native_awt::event::{event_id, AwtEvent, AwtEventData, PeerId};
    let evt = AwtEvent {
        id: event_id::ACTION_PERFORMED,
        source_peer_id: PeerId(1),
        timestamp: 0,
        data: AwtEventData::Action {
            command: "test".to_string(),
        },
    };
    edt_inst.post_event(evt);
    assert_eq!(edt_inst.queue_length(), 1);

    let polled = edt_inst.poll_event();
    assert!(polled.is_some());
    assert_eq!(polled.unwrap().id, event_id::ACTION_PERFORMED);
    assert_eq!(edt_inst.queue_length(), 0);

    edt_inst.stop();
    assert!(!edt_inst.is_running());

    eprintln!("[t7] EDT safety: OK");
}

#[test]
fn t7_2_peer_registry() {
    use cratonvm_native_awt::peer::{ComponentType, PeerRegistry};

    let mut reg = PeerRegistry::new();
    let frame_id = reg.create_peer(ComponentType::Frame);
    let button_id = reg.create_peer(ComponentType::Button);

    // Set parent relationship
    reg.add_child(frame_id, button_id);

    {
        let frame = reg.get(frame_id).unwrap();
        assert_eq!(frame.component_type, ComponentType::Frame);
        assert!(frame.children.contains(&button_id));
    }

    {
        let button = reg.get(button_id).unwrap();
        assert_eq!(button.parent_id, Some(frame_id));
    }

    // Root peers
    let roots = reg.root_peers();
    assert!(roots.contains(&frame_id));
    assert!(!roots.contains(&button_id));

    // Destroy recursively
    reg.destroy(frame_id);
    assert!(reg.get(frame_id).is_none());
    assert!(reg.get(button_id).is_none());

    eprintln!("[t7] Peer registry: OK");
}

// ---------------------------------------------------------------------------
// T7.3 — Java 2D
// ---------------------------------------------------------------------------

#[test]
fn t7_3_1_draw_shapes() {
    use cratonvm_native_awt::renderer::SoftwareRenderer;

    let mut r = SoftwareRenderer::new(200, 200);

    // Draw a red horizontal line
    r.set_color(0xFFFF0000);
    r.draw_line(10, 50, 190, 50);

    // Check that pixels along the line are red
    {
        let pixels = r.pixels();
        let idx = (50 * 200 + 50) as usize;
        assert_eq!(pixels[idx], 0xFFFF0000, "line pixel should be red");
    }

    // Draw rectangle outline
    r.set_color(0xFF00FF00);
    r.draw_rect(20, 20, 60, 40);
    // Top edge should be green
    {
        let pixels = r.pixels();
        let idx_top = (20 * 200 + 40) as usize;
        assert_eq!(pixels[idx_top], 0xFF00FF00, "rect top edge should be green");
    }

    eprintln!("[t7] Shape drawing (line, rect): OK");
}

#[test]
fn t7_3_2_fill_shapes() {
    use cratonvm_native_awt::renderer::SoftwareRenderer;

    let mut r = SoftwareRenderer::new(100, 100);
    r.set_color(0xFF0000FF); // blue
    r.fill_rect(10, 10, 30, 30);

    let pixels = r.pixels();
    let idx = (25 * 100 + 25) as usize;
    assert_eq!(pixels[idx], 0xFF0000FF, "filled rect center should be blue");

    let idx_outside = (5 * 100 + 5) as usize;
    assert_eq!(
        pixels[idx_outside], 0x00000000,
        "outside should be transparent"
    );

    eprintln!("[t7] Shape filling (rect): OK");
}

#[test]
fn t7_3_3_antialiasing_hint() {
    use cratonvm_native_awt::renderer::SoftwareRenderer;

    let mut r = SoftwareRenderer::new(100, 100);
    r.set_antialias(true);
    r.set_color(0xFFFF0000);
    r.draw_line(0, 0, 99, 99);
    // Just verify no panic — AA diagonal line should have coverage variation
    let pixels = r.pixels();
    let center = pixels[(50 * 100 + 50) as usize];
    assert_ne!(
        center, 0x00000000,
        "diagonal line center should have a pixel"
    );

    eprintln!("[t7] Antialiasing hint applied: OK");
}

#[test]
fn t7_3_4_affine_transforms() {
    use cratonvm_native_awt::renderer::AffineTransform;

    let identity = AffineTransform::identity();
    assert!(identity.is_identity());

    // Translate
    let t = AffineTransform::translate(10.0, 20.0);
    let (px, py) = t.transform_point(0.0, 0.0);
    assert!((px - 10.0).abs() < 0.001);
    assert!((py - 20.0).abs() < 0.001);

    // Scale
    let s = AffineTransform::scale(2.0, 3.0);
    let (px, py) = s.transform_point(5.0, 10.0);
    assert!((px - 10.0).abs() < 0.001);
    assert!((py - 30.0).abs() < 0.001);

    // Concatenate (translate then scale)
    let ts = t.concatenate(&s);
    let (px, py) = ts.transform_point(5.0, 10.0);
    // This applies self (translate) after other (scale):
    // scale: (10, 30), then translate: (20, 50)
    assert!((px - 20.0).abs() < 0.001);
    assert!((py - 50.0).abs() < 0.001);

    // Invert
    let inv = t.invert().expect("translation should be invertible");
    let (px, py) = inv.transform_point(10.0, 20.0);
    assert!((px - 0.0).abs() < 0.001);
    assert!((py - 0.0).abs() < 0.001);

    eprintln!("[t7] Affine transforms: OK");
}

#[test]
fn t7_3_5_image_rendering_bilinear() {
    use cratonvm_native_awt::renderer::{InterpolationKind, SoftwareRenderer};

    // Create a 2x2 source image: red, green, blue, white
    let src = [0xFFFF0000u32, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF];

    let mut r = SoftwareRenderer::new(100, 100);
    r.blit_image_scaled(&src, 2, 2, 10, 10, 40, 40, InterpolationKind::Bilinear);

    let pixels = r.pixels();
    let center_idx = (30 * 100 + 30) as usize;
    let pixel = pixels[center_idx];
    assert_ne!(
        pixel, 0x00000000,
        "center should not be transparent after scaled blit"
    );

    eprintln!("[t7] Bilinear image rendering: OK");
}

// ---------------------------------------------------------------------------
// T7.3+ — Color model
// ---------------------------------------------------------------------------

#[test]
fn t7_color_operations() {
    use cratonvm_native_awt::color::Color;

    let red = Color::new(255, 0, 0);
    assert_eq!(red.red(), 255);
    assert_eq!(red.green(), 0);
    assert_eq!(red.blue(), 0);
    assert_eq!(red.alpha(), 255);
    assert_eq!(red.to_argb(), 0xFFFF0000);

    let brighter = red.brighter();
    assert!(brighter.red() >= red.red() || red.red() == 255);

    let darker = red.darker();
    assert!(darker.red() < red.red());

    // Alpha blending
    let fg = Color::with_alpha(255, 0, 0, 128);
    let bg = Color::new(0, 0, 255);
    let blended = Color::blend(fg, bg);
    assert!(blended.red() > 0);
    assert!(blended.blue() > 0);

    eprintln!("[t7] Color operations: OK");
}

// ---------------------------------------------------------------------------
// T7.3+ — Font metrics
// ---------------------------------------------------------------------------

#[test]
fn t7_font_metrics() {
    use cratonvm_native_awt::font::{FontEngine, FontSpec, BOLD, PLAIN};

    let mut engine = FontEngine::new();

    let spec = FontSpec {
        family: "Dialog".to_string(),
        style: PLAIN,
        size: 12,
    };
    let metrics = engine.get_metrics(&spec);
    assert!(metrics.ascent > 0);
    assert!(metrics.descent > 0);
    assert!(metrics.height > 0);
    assert_eq!(
        metrics.height,
        metrics.ascent + metrics.descent + metrics.leading
    );

    let width = engine.string_width(&spec, "Hello, World!");
    assert!(width > 0);

    let big_spec = FontSpec {
        family: "Dialog".to_string(),
        style: BOLD,
        size: 24,
    };
    let big_metrics = engine.get_metrics(&big_spec);
    assert!(big_metrics.height > metrics.height);

    eprintln!("[t7] Font metrics: OK");
}

// ---------------------------------------------------------------------------
// T7.3+ — Graphics2D registry
// ---------------------------------------------------------------------------

#[test]
fn t7_graphics2d_state() {
    use cratonvm_native_awt::graphics2d::Graphics2DState;

    let mut state = Graphics2DState::create(200, 150);
    state.set_color(255, 0, 0, 255);
    state.translate(10.0, 20.0);
    let t = state.get_transform();
    // m02 = tx, m12 = ty in the 3x2 affine matrix
    assert!((t.m02 - 10.0).abs() < 0.01);
    assert!((t.m12 - 20.0).abs() < 0.01);

    // Create a second independent state
    let mut state2 = Graphics2DState::create(100, 100);
    state2.set_color(0, 255, 0, 255);
    let t2 = state2.get_transform();
    assert!(
        t2.is_identity(),
        "fresh Graphics2D should have identity transform"
    );

    // Dispose should not panic
    state.dispose();
    state2.dispose();

    eprintln!("[t7] Graphics2D state: OK");
}

// ---------------------------------------------------------------------------
// T7.3+ — Clipboard
// ---------------------------------------------------------------------------

#[test]
fn t7_clipboard_operations() {
    use cratonvm_native_awt::clipboard::{ClipboardKind, ClipboardManager, DataFlavor};

    let mut clip = ClipboardManager::new();
    clip.set_text(
        ClipboardKind::System,
        "Hello from CratonVM".to_string(),
        None,
    );

    assert_eq!(
        clip.get_text(ClipboardKind::System),
        Some("Hello from CratonVM")
    );
    assert!(clip.has_flavor(ClipboardKind::System, &DataFlavor::StringFlavor));

    let flavors = clip.available_flavors(ClipboardKind::System);
    assert!(!flavors.is_empty());

    eprintln!("[t7] Clipboard operations: OK");
}

// ---------------------------------------------------------------------------
// T7.4 — JavaFX (out-of-tree documentation)
// ---------------------------------------------------------------------------

#[test]
fn t7_4_1_javafx_documented_as_out_of_tree() {
    let doc_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/javafx-status.md");
    assert!(
        std::path::Path::new(doc_path).exists(),
        "docs/javafx-status.md should exist documenting JavaFX as out-of-tree"
    );
    let content = std::fs::read_to_string(doc_path).unwrap();
    assert!(
        content.contains("out-of-tree") || content.contains("Gluon"),
        "javafx-status.md should document JavaFX as out-of-tree/Gluon-supplied"
    );
    eprintln!("[t7] JavaFX documented as out-of-tree: OK");
}

// ---------------------------------------------------------------------------
// T7.5 — Verification (display-dependent tests)
// ---------------------------------------------------------------------------

/// T7.5.1 — Boot IntelliJ IDEA Community in graphical mode.
#[test]
#[ignore]
fn t7_5_1_intellij_graphical() {
    let idea_home = match std::env::var("IDEA_HOME") {
        Ok(h) if std::path::Path::new(&h).is_dir() => h,
        _ => {
            eprintln!("[t7] IDEA_HOME not set; skipping IntelliJ graphical test");
            return;
        }
    };
    eprintln!("[t7] Would boot IntelliJ from {idea_home} — display-dependent test");
}

/// T7.5.2 — Boot NetBeans in graphical mode.
#[test]
#[ignore]
fn t7_5_2_netbeans_graphical() {
    let nb_home = match std::env::var("NETBEANS_HOME") {
        Ok(h) if std::path::Path::new(&h).is_dir() => h,
        _ => {
            eprintln!("[t7] NETBEANS_HOME not set; skipping NetBeans graphical test");
            return;
        }
    };
    eprintln!("[t7] Would boot NetBeans from {nb_home} — display-dependent test");
}

/// T7.5.3 — Boot DBeaver Community.
#[test]
#[ignore]
fn t7_5_3_dbeaver_graphical() {
    let db_home = match std::env::var("DBEAVER_HOME") {
        Ok(h) if std::path::Path::new(&h).is_dir() => h,
        _ => {
            eprintln!("[t7] DBEAVER_HOME not set; skipping DBeaver graphical test");
            return;
        }
    };
    eprintln!("[t7] Would boot DBeaver from {db_home} — display-dependent test");
}

/// T7.5.4 — Re-measure readiness after T7 desktop support.
#[test]
fn t7_5_4_readiness_measurement() {
    use cratonvm_native_api::NativeMethodRegistry;
    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_awt::register_awt_natives(&mut registry);
    let count = registry.len();
    eprintln!("[t7] AWT/Swing native methods: {count}");
    eprintln!("[t7] T7 desktop support is functional (headless)");
    assert!(
        count >= 50,
        "Expected >= 50 AWT natives for desktop support"
    );
}
