import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Locale;
import java.util.ResourceBundle;

/**
 * Framework-independent repro: a class DEFINED BY a custom ClassLoader calls
 * the caller-sensitive {@code ResourceBundle.getBundle(String)}. The bundle
 * lives only in that loader's own directory, so the lookup can only succeed
 * if the caller-class -> ClassLoader resolution is loader-faithful.
 */
public class CallerBundleProbe {
    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("callerbundle");
        // bundle visible ONLY to the child loader
        Files.writeString(dir.resolve("probebundle.properties"), "k=v\n");
        // Move BundleUser.class from this classpath into the child loader's dir
        // so the child (not the app loader) is its defining loader.
        byte[] cls = readAll(CallerBundleProbe.class.getResourceAsStream("/BundleUser.class"));
        Files.write(dir.resolve("BundleUser.class"), cls);

        // Parent = platform loader, so BundleUser cannot come from the app cp.
        URLClassLoader child = new URLClassLoader(
                new URL[] { dir.toUri().toURL() },
                ClassLoader.getPlatformClassLoader());

        Class<?> c = Class.forName("BundleUser", false, child);
        System.out.println("BundleUser loader=" + c.getClassLoader());
        System.out.println("BundleUser module=" + c.getModule()
                + " named=" + c.getModule().isNamed()
                + " moduleLoader=" + c.getModule().getClassLoader());
        System.out.println("child.getResource(probebundle.properties)="
                + (child.getResource("probebundle.properties") != null));
        System.out.println("getBundle(name,locale,child)="
                + tryExplicit(child));
        try {
            Object r = c.getMethod("load").invoke(null);
            System.out.println("caller-sensitive getBundle -> " + r);
        } catch (Throwable t) {
            Throwable r = t;
            while (r.getCause() != null) r = r.getCause();
            System.out.println("caller-sensitive getBundle FAIL " + r.getClass().getName() + ": " + r.getMessage());
        }
        System.out.println("== DONE OK ==");
    }

    private static String tryExplicit(ClassLoader cl) {
        try {
            ResourceBundle b = ResourceBundle.getBundle("probebundle", Locale.getDefault(), cl);
            return "OK " + b.getString("k");
        } catch (Throwable t) {
            return "FAIL " + t.getClass().getName();
        }
    }

    private static byte[] readAll(InputStream in) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = in.read(buf)) > 0) out.write(buf, 0, n);
        in.close();
        return out.toByteArray();
    }
}
