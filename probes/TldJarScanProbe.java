import java.io.File;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * Reproduces, outside Tomcat, the exact JAR walk that dominates
 * TomcatServletWebServerFactoryTests / JettyServletWebServerFactoryTests:
 * Jasper's TldScanner drives `org.apache.tomcat.util.scan.JarFileUrlJar`,
 * whose `nextEntry()` is
 *
 *     if (entries == null) { entries = jarFile.entries(); }
 *     if (multiRelease) {
 *         ... entry = jarFile.getJarEntry(entry.getName());   // per entry!
 *     } else {
 *         entry = entries.nextElement();
 *     }
 *
 * so a multi-release jar costs one `getJarEntry(String)` per entry, and the
 * whole walk is repeated once per embedded-container start (121 of them in
 * the Tomcat class).
 *
 * Two numbers matter and this probe prints both:
 *
 *   * `multiRelease` per jar, so a wrong answer here (which would put every
 *     jar on the expensive branch) is visible rather than inferred; and
 *   * the wall time of ROUNDS repetitions of the walk, so the per-lookup cost
 *     is measured rather than reasoned about.
 *
 * usage: TldJarScanProbe <classpath-string> [rounds]
 */
public class TldJarScanProbe {

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: TldJarScanProbe <classpath> [rounds]");
            return;
        }
        String[] parts = args[0].split(File.pathSeparator);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;

        int jars = 0;
        int mr = 0;
        long totalEntries = 0;
        for (String p : parts) {
            File f = new File(p);
            if (!f.isFile() || !p.toLowerCase().endsWith(".jar")) {
                continue;
            }
            jars++;
            try (JarFile jf = new JarFile(f, true, JarFile.OPEN_READ, Runtime.version())) {
                if (jf.isMultiRelease()) {
                    mr++;
                }
                totalEntries += jf.size();
            } catch (Throwable t) {
                System.out.println("OPEN-FAIL " + f.getName() + " : " + t);
            }
        }
        System.out.println("PROBE jars=" + jars + " multiRelease=" + mr + " entries=" + totalEntries);

        for (int round = 0; round < rounds; round++) {
            long openNs = 0;
            long enumNs = 0;
            long lookupNs = 0;
            long looked = 0;
            long walked = 0;
            long t0 = System.nanoTime();
            for (String p : parts) {
                File f = new File(p);
                if (!f.isFile() || !p.toLowerCase().endsWith(".jar")) {
                    continue;
                }
                long a = System.nanoTime();
                try (JarFile jf = new JarFile(f, true, JarFile.OPEN_READ, Runtime.version())) {
                    boolean multiRelease = jf.isMultiRelease();
                    long b = System.nanoTime();
                    openNs += b - a;
                    Enumeration<JarEntry> en = jf.entries();
                    while (en.hasMoreElements()) {
                        JarEntry e = en.nextElement();
                        walked++;
                        if (multiRelease) {
                            long c = System.nanoTime();
                            enumNs += c - b;
                            // The JarFileUrlJar.nextEntry() re-lookup.
                            JarEntry resolved = jf.getJarEntry(e.getName());
                            if (resolved != null) {
                                looked++;
                            }
                            b = System.nanoTime();
                            lookupNs += b - c;
                        }
                    }
                    if (!multiRelease) {
                        enumNs += System.nanoTime() - b;
                    }
                } catch (Throwable t) {
                    // counted by the census above; keep the timing loop going
                }
            }
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            System.out.println("ROUND " + round + " ms=" + ms + " walked=" + walked + " relookups=" + looked
                    + " openMs=" + (openNs / 1_000_000L) + " enumMs=" + (enumNs / 1_000_000L) + " lookupMs="
                    + (lookupNs / 1_000_000L));
        }
    }
}
