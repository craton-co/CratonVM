// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L5 -- the cold-branch variant of
// `L5W40BootParentGlobalName` (`i40-L5-proposal-a-loaders-global-name-view-is-three-valued`,
// "Wave 118"). A JDK-global-looking name (`javax.l5w118.Lazy`) that is NOT in
// the runtime image but IS on the application class path, referenced by a
// class of a loader whose parent chain never reaches the application loader
// (null parent, or the platform loader) and which overrides only `findClass`.
// HotSpot asks that loader, so its own `findClass` decides (it defines its
// own copy, or throws). The reference sits in a COLD branch of a method that
// runs 30 000 times with the branch not taken first (so it is compiled with the
// site unresolved), then once with it taken: the JIT's cold-site rule
// (`jit_bridge.rs` `jit_user_loader_global_name_change`, one bool per loader)
// is what decides it there, where the interpreter's door asks per name.
//
// Setup (once, before the run; it writes `javax/l5w118/Lazy.class` next to the
// probe's own classes, so the application class path has it):
//   javac -d out L5W118BootParentGlobalNameColdSite.java
//   java -cp out L5W118BootParentGlobalNameColdSite setup
// Run (both modes; JIT is the point, --nojit is the control):
//   java -cp out L5W118BootParentGlobalNameColdSite                  (HotSpot; also -Xint)
//   cratonvm --jdk-only -cp out L5W118BootParentGlobalNameColdSite
//   cratonvm --jdk-only --nojit -cp out L5W118BootParentGlobalNameColdSite
//   cratonvm --compatible -cp out L5W118BootParentGlobalNameColdSite
// A difference from the block below in a JIT run but not under --nojit is the
// cold-site defect the page names: file it.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint identical)
// null-parent warm=30000 own=true
// null-parent-ise warm=30000 =java.lang.IllegalStateException
// platform-parent warm=30000 own=true
// app-parent warm=30000 own=false
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W118BootParentGlobalNameColdSite {
    static final String P = "L5W118BootParentGlobalNameColdSite$";
    static final String LAZY_NESTED = "L5W118BootParentGlobalNameColdSite$Lazy";
    static final String LAZY = "javax/l5w118/Lazy";

    /** `bytes` with every CONSTANT_Utf8 equal to `from` replaced by `to`. */
    static byte[] renameUtf8(byte[] bytes, String from, String to) {
        byte[] f = from.getBytes(StandardCharsets.UTF_8);
        byte[] t = to.getBytes(StandardCharsets.UTF_8);
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        int i = 0;
        while (i < bytes.length) {
            boolean hit = i + 3 + f.length <= bytes.length
                    && bytes[i] == 1
                    && ((bytes[i + 1] & 0xff) << 8 | (bytes[i + 2] & 0xff)) == f.length
                    && java.util.Arrays.equals(bytes, i + 3, i + 3 + f.length, f, 0, f.length);
            if (hit) {
                out.write(1);
                out.write(t.length >> 8);
                out.write(t.length & 0xff);
                out.write(t, 0, t.length);
                i += 3 + f.length;
            } else {
                out.write(bytes[i]);
                i++;
            }
        }
        return out.toByteArray();
    }

    static byte[] own(String internal) throws IOException {
        try (InputStream in = L5W118BootParentGlobalNameColdSite.class.getClassLoader()
                .getResourceAsStream(internal + ".class")) {
            return in.readAllBytes();
        }
    }

    static byte[] lazyBytes() throws IOException {
        return renameUtf8(own(LAZY_NESTED), LAZY_NESTED, LAZY);
    }

    /** Overrides `findClass` only. */
    static final class FindOnly extends ClassLoader {
        final boolean refuse;

        FindOnly(ClassLoader parent, boolean refuse) {
            super(parent);
            this.refuse = refuse;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            try {
                if (name.equals("javax.l5w118.Lazy")) {
                    if (refuse) {
                        throw new IllegalStateException("refused " + name);
                    }
                    byte[] b = lazyBytes();
                    return defineClass(name, b, 0, b.length);
                }
                if (name.equals(P + "Ref")) {
                    byte[] b = renameUtf8(own(P.replace('.', '/') + "Ref"), LAZY_NESTED, LAZY);
                    return defineClass(name, b, 0, b.length);
                }
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
            throw new ClassNotFoundException(name);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("setup")) {
            Path root = Path.of(L5W118BootParentGlobalNameColdSite.class.getProtectionDomain()
                    .getCodeSource().getLocation().toURI());
            Path file = root.resolve(LAZY + ".class");
            Files.createDirectories(file.getParent());
            Files.write(file, lazyBytes());
            System.out.println("wrote " + file);
            return;
        }
        Object[][] rows = {
            {"null-parent", null, false},
            {"null-parent-ise", null, true},
            {"platform-parent", ClassLoader.getPlatformClassLoader(), false},
            {"app-parent", L5W118BootParentGlobalNameColdSite.class.getClassLoader(), false},
        };
        for (Object[] row : rows) {
            String label = (String) row[0];
            FindOnly loader = new FindOnly((ClassLoader) row[1], (Boolean) row[2]);
            Class<?> ref = loader.loadClass(P + "Ref");
            java.lang.reflect.Method m = ref.getMethod("make", boolean.class);
            int warm = 0;
            for (int i = 0; i < 30000; i++) {
                if (m.invoke(null, false) == null) {
                    warm++;
                }
            }
            try {
                Object lazy = m.invoke(null, true);
                System.out.println(label + " warm=" + warm + " own="
                        + (lazy.getClass().getClassLoader() == loader));
            } catch (java.lang.reflect.InvocationTargetException e) {
                System.out.println(label + " warm=" + warm + " =" + e.getCause().getClass().getName());
            }
        }
    }

    public static class Lazy {
        public Lazy() {
        }
    }

    public static class Ref {
        static int sink;

        /** The `new Lazy()` site is cold until `take`. */
        public static Object make(boolean take) {
            int acc = 0;
            for (int i = 0; i < 8; i++) {
                acc += i * 3;
            }
            sink += acc;
            if (take) {
                return new Lazy();
            }
            return null;
        }
    }
}
