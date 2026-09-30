// Interpreter round i1, wave 40, lane L5 -- a JDK-global-looking name
// (`javax.l5w40.Lazy`) that is NOT in the runtime image but IS on the
// application class path, referenced by a class of a loader whose parent
// chain never reaches the application loader (null parent, or the platform
// loader) and which overrides only `findClass`. HotSpot asks that loader
// (its parents cannot see the class path), so the loader's own `findClass`
// decides: it defines its own copy, or throws.
//
// CratonVM before wave 40 (from the code): `constants.rs`
// `drive_loader_for_global_name` calls such a loader "transparent" (no
// `loadClass` override, every parent built-in or the bootstrap), never asks
// it, and the global route answers the APPLICATION loader's `Lazy` from the
// class path: `null-parent own=false`, `null-parent-ise=ok own=false`,
// `platform-parent own=false`. `app-parent own=false` is right (the parent
// answers first on HotSpot too).
//
// Setup (once, before the run; it writes `javax/l5w40/Lazy.class` next to
// the probe's own classes, so the application class path has it):
//   javac -d out L5W40BootParentGlobalName.java
//   java -cp out L5W40BootParentGlobalName setup
// Run:
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W40BootParentGlobalName
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] GLOBAL-NAME ASK javax.l5w40.Lazy: ...` for the three rows
// whose loader does not reach the application loader.
// `--compatible` is unchanged by design (the global route answers:
// `own=false` on every row, `null-parent-ise=ok own=false`).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   null-parent own=true
//   null-parent-ise=java.lang.IllegalStateException
//   platform-parent own=true
//   app-parent own=false

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W40BootParentGlobalName {
    static final String P = "L5W40BootParentGlobalName$";
    static final String LAZY_NESTED = "L5W40BootParentGlobalName$Lazy";
    static final String LAZY = "javax/l5w40/Lazy";

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
        try (InputStream in = L5W40BootParentGlobalName.class.getClassLoader()
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
                if (name.equals("javax.l5w40.Lazy")) {
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
            Path root = Path.of(L5W40BootParentGlobalName.class.getProtectionDomain()
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
            {"app-parent", L5W40BootParentGlobalName.class.getClassLoader(), false},
        };
        for (Object[] row : rows) {
            String label = (String) row[0];
            FindOnly loader = new FindOnly((ClassLoader) row[1], (Boolean) row[2]);
            Class<?> ref = loader.loadClass(P + "Ref");
            try {
                Object lazy = ref.getMethod("make").invoke(null);
                System.out.println(label + (label.endsWith("-ise") ? "=ok" : "") + " own="
                        + (lazy.getClass().getClassLoader() == loader));
            } catch (java.lang.reflect.InvocationTargetException e) {
                System.out.println(label + "=" + e.getCause().getClass().getName());
            }
        }
    }

    public static class Lazy {
        public Lazy() {
        }
    }

    public static class Ref {
        public static Object make() {
            return new Lazy();
        }
    }
}
