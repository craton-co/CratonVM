import java.io.File;
import java.io.FileOutputStream;
import java.net.URL;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

/**
 * Repro for the ChangeableUrlsTests.urlsFromJarClassPathAreConsidered residual.
 *
 * Mirrors ChangeableUrls.getUrlsFromManifestClassPathAttribute: for each
 * manifest Class-Path entry, `new URL(jarUrl, entry)` then
 * `new File(referenced.getFile()).exists()`, falling back to the
 * URLDecoder-decoded entry.
 */
public class DevtoolsUrlCtxProbe {

    static int failures = 0;

    static void check(String what, Object actual, Object expected) {
        boolean ok = (expected == null) ? actual == null : expected.equals(actual);
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + " = " + actual
                + (ok ? "" : "   (expected " + expected + ")"));
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        File tempDir = new File(System.getProperty("java.io.tmpdir"),
                "devtools-urlctx-" + System.nanoTime());
        tempDir.mkdirs();

        File jar = new File(tempDir, "classpath.jar");
        Manifest manifest = new Manifest();
        manifest.getMainAttributes().putValue(Attributes.Name.MANIFEST_VERSION.toString(), "1.0");
        manifest.getMainAttributes().putValue(Attributes.Name.CLASS_PATH.toString(),
                "project-core/target/classes/ project%20space/target/classes/ does-not-exist/target/classes/");
        new JarOutputStream(new FileOutputStream(jar), manifest).close();

        new File(tempDir, "project-core/target/classes").mkdirs();
        new File(tempDir, "project space/target/classes").mkdirs();

        URL jarUrl = jar.toURI().toURL();
        System.out.println("jarUrl        = " + jarUrl);
        System.out.println("jarUrl.getFile= " + jarUrl.getFile());
        System.out.println();

        // --- 1. context-relative URL construction must not re-escape '%' ---
        System.out.println("[1] new URL(context, spec) escaping");
        URL plain = new URL(jarUrl, "project-core/target/classes/");
        check("new URL(ctx, \"project-core/target/classes/\")", plain.toString(),
                relative(jarUrl, "project-core/target/classes/"));
        URL pct = new URL(jarUrl, "project%20space/target/classes/");
        check("new URL(ctx, \"project%20space/target/classes/\")", pct.toString(),
                relative(jarUrl, "project%20space/target/classes/"));
        URL space = new URL(jarUrl, "project space/target/classes/");
        check("new URL(ctx, \"project space/target/classes/\")", space.toString(),
                relative(jarUrl, "project space/target/classes/"));
        URL missing = new URL(jarUrl, "does-not-exist/target/classes/");
        check("new URL(ctx, \"does-not-exist/target/classes/\")", missing.toString(),
                relative(jarUrl, "does-not-exist/target/classes/"));
        System.out.println();

        // --- 2. getFile() round-trip ---
        System.out.println("[2] getFile()");
        System.out.println("  plain.getFile()   = " + plain.getFile());
        System.out.println("  pct.getFile()     = " + pct.getFile());
        System.out.println("  space.getFile()   = " + space.getFile());
        System.out.println("  missing.getFile() = " + missing.getFile());
        System.out.println();

        // --- 3. File.exists() on the URL file component ---
        System.out.println("[3] new File(url.getFile()).exists()");
        check("exists(project-core)  ", new File(plain.getFile()).exists(), Boolean.TRUE);
        check("exists(project%20space)", new File(pct.getFile()).exists(), Boolean.FALSE);
        check("exists(project space) ", new File(space.getFile()).exists(), Boolean.TRUE);
        check("exists(does-not-exist)", new File(missing.getFile()).exists(), Boolean.FALSE);
        System.out.println();

        // --- 4. URLDecoder ---
        System.out.println("[4] URLDecoder.decode");
        check("decode(project%20space/target/classes/)",
                URLDecoder.decode("project%20space/target/classes/", StandardCharsets.UTF_8),
                "project space/target/classes/");
        System.out.println();

        // --- 5. the real algorithm ---
        System.out.println("[5] ChangeableUrls algorithm");
        String[] entries = { "project-core/target/classes/", "project%20space/target/classes/",
                "does-not-exist/target/classes/" };
        StringBuilder kept = new StringBuilder();
        for (String entry : entries) {
            URL referenced = new URL(jarUrl, entry);
            if (new File(referenced.getFile()).exists()) {
                kept.append(referenced).append('\n');
            }
            else {
                referenced = new URL(jarUrl, URLDecoder.decode(entry, StandardCharsets.UTF_8));
                if (new File(referenced.getFile()).exists()) {
                    kept.append(referenced).append('\n');
                }
            }
        }
        String expected = relative(jarUrl, "project-core/target/classes/") + "\n"
                + relative(jarUrl, "project space/target/classes/") + "\n";
        check("kept URLs", "\n" + kept, "\n" + expected);

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        deleteRec(tempDir);
        System.exit(failures == 0 ? 0 : 1);
    }

    /** Expected external form of a context-relative resolve, computed by string surgery. */
    private static String relative(URL ctx, String spec) {
        String s = ctx.toString();
        return s.substring(0, s.lastIndexOf('/') + 1) + spec;
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
