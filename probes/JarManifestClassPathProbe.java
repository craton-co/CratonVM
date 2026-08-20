import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

/**
 * A jar whose manifest carries `Class-Path:` must let its named jars satisfy a
 * class load.
 *
 * This is the user-visible behaviour behind
 * `jdk.internal.access.JavaUtilJarAccess.jarFileHasClassPathAttribute`, which
 * gates `jdk.internal.loader.URLClassPath$JarLoader.getClassPath()`. CratonVM
 * hardcoded that predicate to `false` under the comment "Returning false is
 * spec-compatible — callers short-circuit the full attribute scan". A `false`
 * is not a cheaper `true`: it tells the loader no jar in the process has the
 * attribute, and the caller cannot tell it was not asked.
 *
 * The probe builds both jars at runtime so it depends on no fixture:
 *
 *   dep.jar   contains `Dep.class`, and nothing references it
 *   main.jar  contains ONLY a manifest with `Class-Path: dep.jar`
 *
 * then loads `Dep` through a `URLClassLoader` over **main.jar alone**, with a
 * `null` parent so the application class path cannot answer instead. On
 * HotSpot the manifest entry resolves it. That is the oracle; run both.
 *
 * Prints `RESULT loaded=<bool> attr=<bool>` and exits non-zero on a miss, so a
 * harness can gate on it. `attr` is the same question asked directly through
 * `JarFile`, which is what the predicate should be reading — if `attr=true` and
 * `loaded=false`, the manifest is fine and the loader was told otherwise.
 */
public final class JarManifestClassPathProbe {

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("craton-cp-probe");
        Path depJar = dir.resolve("dep.jar");
        Path mainJar = dir.resolve("main.jar");

        byte[] depClass = readOwnResource("Dep.class");
        if (depClass == null) {
            System.out.println("SKIP Dep.class not on the probe's own class path — "
                    + "compile Dep.java beside this file");
            System.exit(2);
        }

        // dep.jar: the class, no manifest.
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(depJar))) {
            out.putNextEntry(new JarEntry("Dep.class"));
            out.write(depClass);
            out.closeEntry();
        }

        // main.jar: a manifest with Class-Path, and NOTHING else. If the
        // attribute is ignored there is nothing here to load.
        Manifest manifest = new Manifest();
        manifest.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        manifest.getMainAttributes().putValue("Class-Path", "dep.jar");
        try (JarOutputStream out =
                     new JarOutputStream(Files.newOutputStream(mainJar), manifest)) {
            // no entries on purpose
        }

        // The same question asked directly, as the control on the fixture: if
        // this is false the jars were built wrong and the load below proves
        // nothing.
        boolean attr;
        try (JarFile jf = new JarFile(mainJar.toFile())) {
            Manifest m = jf.getManifest();
            String v = m == null ? null : m.getMainAttributes().getValue("Class-Path");
            attr = v != null && !v.trim().isEmpty();
        }

        boolean loaded;
        Throwable failure = null;
        // `null` parent: the platform loader, NOT the application class path —
        // otherwise `Dep` resolves from the classpath this probe was started
        // with and the test passes without the manifest being read at all.
        try (URLClassLoader cl =
                     new URLClassLoader(new URL[] {mainJar.toUri().toURL()}, null)) {
            Class<?> c = Class.forName("Dep", true, cl);
            loaded = c != null && "Dep".equals(c.getName());
        } catch (Throwable t) {
            loaded = false;
            failure = t;
        }

        System.out.println("RESULT loaded=" + loaded + " attr=" + attr
                + " predicate=" + askPredicate(mainJar));
        if (failure != null) {
            System.out.println("       failure=" + failure);
        }
        if (!attr) {
            System.out.println("FAILED the fixture is wrong: main.jar's own manifest "
                    + "does not report Class-Path, so the load below proves nothing");
            System.exit(1);
        }
        if (!loaded) {
            System.out.println("FAILED a jar's manifest Class-Path did not satisfy a "
                    + "class load — jarFileHasClassPathAttribute is answering for the "
                    + "loader instead of asking the manifest");
            System.exit(1);
        }
        System.out.println("PASSED");
    }

    /**
     * Ask `JavaUtilJarAccess.jarFileHasClassPathAttribute` DIRECTLY.
     *
     * `loaded` above is the user-visible behaviour, and it can be satisfied by
     * a VM whose own loader expands `Class-Path` without ever consulting this
     * predicate — CratonVM's does. So `loaded` alone cannot tell you whether
     * the predicate is right, and a fix to it would look inert. This line asks
     * the changed thing itself.
     *
     * Needs `--add-exports java.base/jdk.internal.access=ALL-UNNAMED`; without
     * it the answer is `unavailable(...)` and only `loaded` is meaningful.
     */
    private static String askPredicate(Path mainJar) {
        try {
            Class<?> secrets = Class.forName("jdk.internal.access.SharedSecrets");
            Object access = secrets.getMethod("javaUtilJarAccess").invoke(null);
            if (access == null) {
                return "null-access";
            }
            // Interface first, receiver's class second. On HotSpot the
            // interface `Method` works and `setAccessible` would need
            // `--add-opens` on top of `--add-exports`. On CratonVM the carrier
            // is a synthetic class that reflection reports as neither an
            // instance of the interface nor a declarer of the method, so BOTH
            // routes answer `unavailable` — recorded here rather than worked
            // around, because "the predicate cannot be asked from Java on this
            // VM" is the honest state and the next reader should not spend an
            // hour rediscovering it.
            java.lang.reflect.Method predicate;
            try {
                predicate = Class.forName("jdk.internal.access.JavaUtilJarAccess")
                        .getMethod("jarFileHasClassPathAttribute", JarFile.class);
            } catch (ReflectiveOperationException viaInterface) {
                predicate = access
                        .getClass()
                        .getMethod("jarFileHasClassPathAttribute", JarFile.class);
            }
            try (JarFile jf = new JarFile(mainJar.toFile())) {
                return String.valueOf(predicate.invoke(access, jf));
            }
        } catch (Throwable t) {
            Throwable root = t.getCause() == null ? t : t.getCause();
            return "unavailable(" + root.getClass().getSimpleName() + ")";
        }
    }

    /** The probe's own class path holds `Dep.class`; read it as a resource. */
    private static byte[] readOwnResource(String name) throws Exception {
        try (InputStream in = JarManifestClassPathProbe.class
                .getClassLoader()
                .getResourceAsStream(name)) {
            if (in == null) {
                return null;
            }
            ByteArrayOutputStream buf = new ByteArrayOutputStream();
            byte[] chunk = new byte[8192];
            int n;
            while ((n = in.read(chunk)) > 0) {
                buf.write(chunk, 0, n);
            }
            return buf.toByteArray();
        }
    }
}
