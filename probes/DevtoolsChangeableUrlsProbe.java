import java.io.File;
import java.io.FileOutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.jar.Attributes;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.zip.ZipOutputStream;

/**
 * End-to-end mirror of
 * ChangeableUrlsTests.urlsFromJarClassPathAreConsidered, with each stage
 * printed separately so the failing stage is unambiguous:
 *
 *   [A] URLClassLoader.getURLs()  — must return exactly the two jars
 *   [B] JarFile.getManifest() Class-Path — must round-trip the written value
 *   [C] the getUrlsFromManifestClassPathAttribute algorithm
 */
public class DevtoolsChangeableUrlsProbe {

    static int failures = 0;

    static void check(String what, Object actual, Object expected) {
        boolean ok = (expected == null) ? actual == null : expected.equals(actual);
        System.out.println((ok ? "  OK   " : "  FAIL ") + what);
        System.out.println("         actual   = " + actual);
        if (!ok) {
            System.out.println("         expected = " + expected);
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        File tempDir = new File(System.getProperty("java.io.tmpdir"),
                "devtools-changeable-" + System.nanoTime());
        tempDir.mkdirs();

        File relative = new File(tempDir, "rel-dir");
        relative.mkdir();
        File absolute = new File(tempDir, "abs-dir");
        absolute.mkdirs();
        URL absoluteUrl = absolute.toURI().toURL();

        File jar = new File(tempDir, "classpath.jar");
        String classPath = "project-core/target/classes/ project-web/target/classes/"
                + " project%20space/target/classes/ does-not-exist/target/classes/"
                + " " + relative.getName() + "/ " + absoluteUrl;
        Manifest manifest = new Manifest();
        manifest.getMainAttributes().putValue(Attributes.Name.MANIFEST_VERSION.toString(), "1.0");
        manifest.getMainAttributes().putValue(Attributes.Name.CLASS_PATH.toString(), classPath);
        new JarOutputStream(new FileOutputStream(jar), manifest).close();

        File noManifest = new File(tempDir, "no-manifest.jar");
        new ZipOutputStream(new FileOutputStream(noManifest)).close();

        new File(tempDir, "project-core/target/classes").mkdirs();
        new File(tempDir, "project-web/target/classes").mkdirs();
        new File(tempDir, "project space/target/classes").mkdirs();

        URL jarUrl = jar.toURI().toURL();

        // --- [A] URLClassLoader.getURLs() ---
        System.out.println("[A] URLClassLoader.getURLs()");
        URLClassLoader ucl = new URLClassLoader(new URL[] { jarUrl, noManifest.toURI().toURL() });
        URL[] got = ucl.getURLs();
        StringBuilder sb = new StringBuilder();
        for (URL u : got) {
            sb.append("\n           ").append(u);
        }
        check("getURLs()", got.length + " urls:" + sb,
                "2 urls:\n           " + jarUrl + "\n           " + noManifest.toURI().toURL());

        // --- [B] manifest round-trip ---
        System.out.println("[B] JarFile.getManifest() Class-Path");
        try (JarFile jarFile = new JarFile(jar)) {
            Manifest read = jarFile.getManifest();
            check("manifest non-null", read != null, Boolean.TRUE);
            if (read != null) {
                check("Class-Path value",
                        read.getMainAttributes().getValue(Attributes.Name.CLASS_PATH), classPath);
            }
        }

        // --- [C] the ChangeableUrls algorithm ---
        System.out.println("[C] getUrlsFromManifestClassPathAttribute");
        String[] entries = classPath.split(" ");
        List<String> urls = new ArrayList<>();
        List<String> nonExistent = new ArrayList<>();
        for (String entry : entries) {
            URL referenced = new URL(jarUrl, entry);
            if (new File(referenced.getFile()).exists()) {
                urls.add(referenced.toString());
            }
            else {
                URL decoded = new URL(jarUrl, URLDecoder.decode(entry, StandardCharsets.UTF_8));
                if (new File(decoded.getFile()).exists()) {
                    urls.add(decoded.toString());
                }
                else {
                    nonExistent.add(decoded.toString());
                }
            }
        }
        String base = jarUrl.toString().substring(0, jarUrl.toString().lastIndexOf('/') + 1);
        List<String> expected = new ArrayList<>();
        expected.add(base + "project-core/target/classes/");
        expected.add(base + "project-web/target/classes/");
        expected.add(base + "project space/target/classes/");
        expected.add(base + relative.getName() + "/");
        expected.add(absoluteUrl.toString());
        check("kept urls", urls, expected);
        check("non-existent entries", nonExistent,
                List.of(base + "does-not-exist/target/classes/"));

        ucl.close();
        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        deleteRec(tempDir);
        System.exit(failures == 0 ? 0 : 1);
    }

    private static void deleteRec(File f) {
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File k : kids) {
                deleteRec(k);
            }
        }
        f.delete();
    }
}
