import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

/**
 * Reach the `jdk/internal/loader/` rows through PUBLIC API only.
 *
 * The 20 rows of that package were classified and not retired because 19 of
 * them appear in no vector of the 132-vector corpus: retirement precondition 4
 * asks for a dispatch observed by the instrument that produced the
 * improvement, and no instrument reached them. This is that instrument.
 *
 * Nothing here imports `jdk.internal.loader` or uses reflection, so it needs no
 * `--add-opens` / `--add-exports`: every row drives the JDK's own code path and
 * lets the JDK reach its internals itself.
 *
 *   app / platform / system loader resource lookups  -> ClassLoaders$*, URLClassPath
 *   Class.getResourceAsStream on a java.base class   -> BootLoader.findResourceAsStream
 *   URLClassLoader over a directory and over a jar   -> URLClassPath.{Loader,JarLoader}
 *
 * Padded by hand and printed with println: System.out.printf reaches
 * DecimalFormatSymbols -> LocaleProviderAdapter, which throws under
 * CRATONVM_ENFORCE_NATIVE_SHADOW=all and would print zero rows.
 */
public class L7LoaderInternalsSweep {

    static int n = 0;

    static void row(String label, String value) {
        n++;
        StringBuilder sb = new StringBuilder();
        String idx = String.valueOf(n);
        while (idx.length() < 2) idx = "0" + idx;
        sb.append(idx).append("  ").append(label);
        while (sb.length() < 62) sb.append(' ');
        sb.append("= ").append(value);
        System.out.println(sb.toString());
    }

    static String present(Object o) {
        return o == null ? "null" : "present";
    }

    static String read(InputStream in) {
        if (in == null) return "null";
        try {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int k = in.read(buf);
            while (k != -1) {
                bo.write(buf, 0, k);
                k = in.read(buf);
            }
            in.close();
            return "bytes:" + bo.size();
        } catch (Throwable t) {
            return "throw:" + t.getClass().getName();
        }
    }

    static String count(Enumeration<URL> e) {
        if (e == null) return "null";
        int c = 0;
        while (e.hasMoreElements()) {
            e.nextElement();
            c++;
        }
        return "count:" + c;
    }

    static String safe(String what) {
        return what;
    }

    public static void main(String[] args) {
        String self = "L7LoaderInternalsSweep.class";

        // ---- the app loader: a BuiltinClassLoader over a URLClassPath -----
        ClassLoader app = L7LoaderInternalsSweep.class.getClassLoader();
        try {
            row("app.getName()", app == null ? "null" : String.valueOf(app.getName()));
        } catch (Throwable t) {
            row("app.getName()", "throw:" + t.getClass().getName());
        }
        try {
            row("app.getResource(self)", present(app.getResource(self)));
        } catch (Throwable t) {
            row("app.getResource(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("app.getResourceAsStream(self)", read(app.getResourceAsStream(self)));
        } catch (Throwable t) {
            row("app.getResourceAsStream(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("app.getResources(self)", count(app.getResources(self)));
        } catch (Throwable t) {
            row("app.getResources(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("app.getResource(absent)", present(app.getResource("no/such/thing.txt")));
        } catch (Throwable t) {
            row("app.getResource(absent)", "throw:" + t.getClass().getName());
        }

        // ---- the platform loader ------------------------------------------
        ClassLoader plat = null;
        try {
            plat = app == null ? null : app.getParent();
            row("app.getParent().getName()", plat == null ? "null" : String.valueOf(plat.getName()));
        } catch (Throwable t) {
            row("app.getParent().getName()", "throw:" + t.getClass().getName());
        }
        try {
            row("platform.getResource(self)", plat == null ? "null" : present(plat.getResource(self)));
        } catch (Throwable t) {
            row("platform.getResource(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("platform.getResourceAsStream(self)",
                plat == null ? "null" : present(plat.getResourceAsStream(self)));
        } catch (Throwable t) {
            row("platform.getResourceAsStream(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("platform.getParent()", plat == null ? "null" : present(plat.getParent()));
        } catch (Throwable t) {
            row("platform.getParent()", "throw:" + t.getClass().getName());
        }

        // ---- the system loader, through the static entry points ------------
        try {
            row("ClassLoader.getSystemClassLoader()", present(ClassLoader.getSystemClassLoader()));
        } catch (Throwable t) {
            row("ClassLoader.getSystemClassLoader()", "throw:" + t.getClass().getName());
        }
        try {
            row("ClassLoader.getSystemResource(self)", present(ClassLoader.getSystemResource(self)));
        } catch (Throwable t) {
            row("ClassLoader.getSystemResource(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("ClassLoader.getSystemResourceAsStream(self)",
                read(ClassLoader.getSystemResourceAsStream(self)));
        } catch (Throwable t) {
            row("ClassLoader.getSystemResourceAsStream(self)", "throw:" + t.getClass().getName());
        }
        try {
            row("ClassLoader.getSystemResources(self)", count(ClassLoader.getSystemResources(self)));
        } catch (Throwable t) {
            row("ClassLoader.getSystemResources(self)", "throw:" + t.getClass().getName());
        }

        // ---- BootLoader, via a java.base class's own .class resource -------
        // Class.getResourceAsStream on a boot class delegates to the boot
        // loader; .class resources stay readable under module encapsulation.
        try {
            row("Object.class.getResourceAsStream(Object.class)",
                read(Object.class.getResourceAsStream("Object.class")));
        } catch (Throwable t) {
            row("Object.class.getResourceAsStream(Object.class)", "throw:" + t.getClass().getName());
        }
        try {
            row("Object.class.getResource(Object.class)",
                present(Object.class.getResource("Object.class")));
        } catch (Throwable t) {
            row("Object.class.getResource(Object.class)", "throw:" + t.getClass().getName());
        }
        try {
            row("String.class.getResourceAsStream(absent)",
                present(String.class.getResourceAsStream("no-such-resource")));
        } catch (Throwable t) {
            row("String.class.getResourceAsStream(absent)", "throw:" + t.getClass().getName());
        }

        // ---- this class's own resource, through Class rather than loader ---
        try {
            row("self.class.getResourceAsStream(self)",
                read(L7LoaderInternalsSweep.class.getResourceAsStream(self)));
        } catch (Throwable t) {
            row("self.class.getResourceAsStream(self)", "throw:" + t.getClass().getName());
        }

        // ---- URLClassPath over a DIRECTORY ---------------------------------
        Path dir = null;
        try {
            dir = Files.createTempDirectory("l7dir");
            Files.write(dir.resolve("hello.txt"), "hello-from-dir".getBytes("UTF-8"));
        } catch (Throwable t) {
            row("setup:tempdir", "throw:" + t.getClass().getName());
        }
        URLClassLoader dirLoader = null;
        try {
            dirLoader = new URLClassLoader(new URL[] { dir.toUri().toURL() }, plat);
            row("dirLoader.getResource(hello.txt)", present(dirLoader.getResource("hello.txt")));
        } catch (Throwable t) {
            row("dirLoader.getResource(hello.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("dirLoader.getResourceAsStream(hello.txt)",
                read(dirLoader.getResourceAsStream("hello.txt")));
        } catch (Throwable t) {
            row("dirLoader.getResourceAsStream(hello.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("dirLoader.getResources(hello.txt)", count(dirLoader.getResources("hello.txt")));
        } catch (Throwable t) {
            row("dirLoader.getResources(hello.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("dirLoader.getResource(absent)", present(dirLoader.getResource("nope.txt")));
        } catch (Throwable t) {
            row("dirLoader.getResource(absent)", "throw:" + t.getClass().getName());
        }
        try {
            row("dirLoader.getURLs().length", String.valueOf(dirLoader.getURLs().length));
        } catch (Throwable t) {
            row("dirLoader.getURLs().length", "throw:" + t.getClass().getName());
        }

        // ---- URLClassPath over a JAR ---------------------------------------
        Path jar = null;
        try {
            jar = Files.createTempFile("l7jar", ".jar");
            JarOutputStream jo = new JarOutputStream(Files.newOutputStream(jar));
            jo.putNextEntry(new JarEntry("in/jar.txt"));
            jo.write("hello-from-jar".getBytes("UTF-8"));
            jo.closeEntry();
            jo.close();
        } catch (Throwable t) {
            row("setup:tempjar", "throw:" + t.getClass().getName());
        }
        URLClassLoader jarLoader = null;
        try {
            jarLoader = new URLClassLoader(new URL[] { jar.toUri().toURL() }, plat);
            row("jarLoader.getResource(in/jar.txt)", present(jarLoader.getResource("in/jar.txt")));
        } catch (Throwable t) {
            row("jarLoader.getResource(in/jar.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("jarLoader.getResourceAsStream(in/jar.txt)",
                read(jarLoader.getResourceAsStream("in/jar.txt")));
        } catch (Throwable t) {
            row("jarLoader.getResourceAsStream(in/jar.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("jarLoader.getResources(in/jar.txt)", count(jarLoader.getResources("in/jar.txt")));
        } catch (Throwable t) {
            row("jarLoader.getResources(in/jar.txt)", "throw:" + t.getClass().getName());
        }
        try {
            row("jarLoader.getResource(absent)", present(jarLoader.getResource("in/nope.txt")));
        } catch (Throwable t) {
            row("jarLoader.getResource(absent)", "throw:" + t.getClass().getName());
        }
        try {
            jarLoader.loadClass("no.such.Klass");
            row("jarLoader.loadClass(absent)", "loaded");
        } catch (Throwable t) {
            row("jarLoader.loadClass(absent)", "throw:" + t.getClass().getName());
        }
        try {
            row("jarLoader.loadClass(java.lang.String)",
                jarLoader.loadClass("java.lang.String").getName());
        } catch (Throwable t) {
            row("jarLoader.loadClass(java.lang.String)", "throw:" + t.getClass().getName());
        }

        // ---- cleanup, and the row count the diff is read against -----------
        try {
            if (dirLoader != null) dirLoader.close();
            if (jarLoader != null) jarLoader.close();
        } catch (Throwable t) {
            // closing is not what this probe measures
        }
        try {
            if (dir != null) {
                Files.deleteIfExists(dir.resolve("hello.txt"));
                Files.deleteIfExists(dir);
            }
            if (jar != null) Files.deleteIfExists(jar);
        } catch (Throwable t) {
            // ditto
        }
        System.out.println("rows=" + n);
    }
}
