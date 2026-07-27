import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.URI;
import java.net.URL;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;

/**
 * Replaces a jar IN PLACE at the same path and re-reads it. A byte cache keyed
 * on the path alone (with no mtime/size check) serves the FIRST version
 * forever — which is what made Tomcat's redeploy tests read the wrong WAR.
 *
 * Both versions have the SAME entry name and the SAME payload LENGTH, so only
 * a real content re-read (or an mtime-aware cache) can tell them apart.
 */
public class JarRedeployProbe {

    public static void main(String[] args) throws Exception {
        File jar = new File(System.getProperty("java.io.tmpdir"),
                "redeploy-probe-" + System.nanoTime() + ".jar");
        try {
            write(jar, "AAAA");
            String first = readViaUrl(jar);
            String firstJarFile = readViaJarFile(jar);

            // Make sure the two versions cannot share a filesystem timestamp.
            Thread.sleep(1500);
            write(jar, "BBBB");
            String second = readViaUrl(jar);
            String secondJarFile = readViaJarFile(jar);

            System.out.println("URL.openStream : first=" + first + " second=" + second
                    + "  -> " + (("AAAA".equals(first) && "BBBB".equals(second)) ? "OK" : "STALE"));
            System.out.println("JarFile        : first=" + firstJarFile + " second=" + secondJarFile
                    + "  -> " + (("AAAA".equals(firstJarFile) && "BBBB".equals(secondJarFile))
                            ? "OK" : "STALE"));
        } finally {
            jar.delete();
        }
    }

    private static void write(File jar, String payload) throws Exception {
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new JarEntry("META-INF/marker.txt"));
            out.write(payload.getBytes("UTF-8"));
            out.closeEntry();
        }
    }

    private static String readViaUrl(File jar) throws Exception {
        URL url = URI.create("jar:" + jar.toURI() + "!/META-INF/marker.txt").toURL();
        try (InputStream in = url.openStream()) {
            return new String(in.readAllBytes(), "UTF-8");
        }
    }

    private static String readViaJarFile(File jar) throws Exception {
        try (JarFile jf = new JarFile(jar)) {
            JarEntry e = jf.getJarEntry("META-INF/marker.txt");
            try (InputStream in = jf.getInputStream(e)) {
                return new String(in.readAllBytes(), "UTF-8");
            }
        }
    }
}
