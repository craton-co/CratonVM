// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 101, lane L5 -- `--enable-preview` is a per-VM
// bit now (`VmConfig::enable_preview`), not a process global the launcher
// pushed into the class-file reader. Two consumers must keep reading the SAME
// bit: `jdk.internal.misc.PreviewFeatures.isEnabled()` (the native
// `isPreviewEnabled`, now `NativeSystemAccess::preview_enabled`) and the class
// manager's define gate for a `69.65535` class file. This probe checks both
// from Java: it prints the library's answer, then defines a copy of its own
// nested class re-stamped as a preview class file (minor 65535).
//
// Run (no setup), twice:
//   javac -d out L5W101PreviewFeaturesEnabled.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] \
//       --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W101PreviewFeaturesEnabled
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] --enable-preview \
//       --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W101PreviewFeaturesEnabled
//
// Expected HotSpot 25 output without --enable-preview:
//   preview-enabled=false
//   preview-define=java.lang.UnsupportedClassVersionError: Preview features are not enabled for L5W101PreviewFeaturesEnabled$Payload (class file version 69.65535). Try running with '--enable-preview'
// Expected HotSpot 25 output with --enable-preview:
//   preview-enabled=true
//   preview-define=L5W101PreviewFeaturesEnabled$Payload
//
// The embedding half (`JNI_CreateJavaVM` with the `--enable-preview` option,
// `VmConfig::with_enable_preview`) has no Java-side probe: it is pinned by
// `libcratonvm`'s `config_from_args_reads_enable_preview` and the class
// manager's `i101_preview_enablement_is_per_class_manager` unit tests.

import java.io.InputStream;

public class L5W101PreviewFeaturesEnabled {
    public static final class Payload {}

    static final class L extends ClassLoader {
        L() {
            super(null);
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> pf = Class.forName("jdk.internal.misc.PreviewFeatures");
        Object on = pf.getMethod("isEnabled").invoke(null);
        System.out.println("preview-enabled=" + on);

        String name = "L5W101PreviewFeaturesEnabled$Payload";
        byte[] cls;
        try (InputStream in = L5W101PreviewFeaturesEnabled.class.getResourceAsStream(name + ".class")) {
            cls = in.readAllBytes();
        }
        // minor_version 65535, major_version 69 (JDK 25).
        cls[4] = (byte) 0xFF;
        cls[5] = (byte) 0xFF;
        cls[6] = 0x00;
        cls[7] = 0x45;
        try {
            Class<?> c = new L().def(name, cls);
            System.out.println("preview-define=" + c.getName());
        } catch (Throwable t) {
            System.out.println("preview-define=" + t);
        }
    }
}
