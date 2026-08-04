import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

import org.apache.tomcat.util.bcel.classfile.ClassParser;

/**
 * Does the Tomcat deploy cost DEGRADE across repeated deploys in one process?
 *
 * `AnnotationScanCostProbe` reports the BEST of three rounds, which is exactly
 * the statistic that hides a monotone trend. The Tomcat suite logs say there is
 * one: `org.apache.naming.TestEnvEntry` starts and stops an embedded Tomcat
 * once per `@Test`, over the same webapp, and its successive start-ups take
 * 118 s, 201 s, 255 s, 275 s, 318 s — same work, 2.7x slower by the fifth.
 * `TestHostConfigAutomaticDeploymentAddition`'s own `HostConfig` timings say
 * the same thing: 104 s, 162 s, 260 s, 298 s.
 *
 * This probe separates the two things a deploy repeats, and prints EVERY round
 * rather than the minimum:
 *
 *   parse : BCEL-parse every `.class` entry of the JARs (no class loading) —
 *           the annotation scan itself.
 *   load  : load every class through a FRESH `URLClassLoader` and close it —
 *           what a redeploy does to the VM's class store.
 *
 * A flat `parse` column with a rising `load` column says the cost is in class
 * loading (or in something that scales with how many classes the store holds),
 * not in the scan. Both rising says the whole VM degrades with store
 * occupancy.
 *
 * Usage: DeployRepeatCostProbe &lt;jar-or-dir-of-jars&gt; [rounds] [mode]
 *        mode = both (default) | parse | load
 */
public class DeployRepeatCostProbe {

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("usage: DeployRepeatCostProbe <jar-or-dir> [rounds] [both|parse|load]");
            return;
        }
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        String mode = args.length > 2 ? args[2] : "both";
        boolean doParse = !"load".equals(mode);
        boolean doLoad = !"parse".equals(mode);

        List<File> jars = new ArrayList<>();
        File f = new File(args[0]);
        if (f.isDirectory()) {
            File[] kids = f.listFiles((d, n) -> n.endsWith(".jar"));
            if (kids != null) {
                java.util.Arrays.sort(kids);
                for (File k : kids) {
                    jars.add(k);
                }
            }
        } else if (f.isFile()) {
            jars.add(f);
        }
        if (jars.isEmpty()) {
            System.out.println("no jars under " + args[0]);
            return;
        }

        List<String> names = classNames(jars);
        System.out.printf("%d jar(s), %d classes, %d rounds%n", jars.size(), names.size(), rounds);
        System.out.printf("%5s | %12s | %12s%n", "round", "parse ms", "load ms");

        URL[] urls = new URL[jars.size()];
        for (int i = 0; i < jars.size(); i++) {
            urls[i] = jars.get(i).toURI().toURL();
        }

        for (int r = 1; r <= rounds; r++) {
            long parseMs = -1;
            long loadMs = -1;
            if (doParse) {
                long t0 = System.nanoTime();
                parseAll(jars);
                parseMs = (System.nanoTime() - t0) / 1_000_000L;
            }
            if (doLoad) {
                long t0 = System.nanoTime();
                loadAll(urls, names);
                loadMs = (System.nanoTime() - t0) / 1_000_000L;
            }
            System.out.printf("%5d | %12d | %12d%n", r, parseMs, loadMs);
        }
    }

    private static List<String> classNames(List<File> jars) throws Exception {
        List<String> out = new ArrayList<>();
        for (File jar : jars) {
            try (JarFile jf = new JarFile(jar)) {
                Enumeration<JarEntry> en = jf.entries();
                while (en.hasMoreElements()) {
                    JarEntry e = en.nextElement();
                    String n = e.getName();
                    if (n.endsWith(".class") && !n.contains("module-info")) {
                        out.add(n.substring(0, n.length() - 6).replace('/', '.'));
                    }
                }
            }
        }
        return out;
    }

    private static void parseAll(List<File> jars) throws Exception {
        for (File jar : jars) {
            try (JarFile jf = new JarFile(jar)) {
                Enumeration<JarEntry> en = jf.entries();
                while (en.hasMoreElements()) {
                    JarEntry e = en.nextElement();
                    if (!e.getName().endsWith(".class")) {
                        continue;
                    }
                    try (InputStream is = jf.getInputStream(e)) {
                        new ClassParser(is).parse();
                    } catch (Exception ignored) {
                        // A malformed or unsupported entry is not what is being
                        // measured; keep walking so every round parses the same
                        // set.
                    }
                }
            }
        }
    }

    /**
     * A fresh loader per round is the point: a redeploy defines the same class
     * names again under a new loader, so the VM's class store grows by the
     * webapp's class count every time.
     */
    private static void loadAll(URL[] urls, List<String> names) throws Exception {
        try (URLClassLoader cl = new URLClassLoader(urls, null)) {
            for (String n : names) {
                try {
                    Class.forName(n, false, cl);
                } catch (Throwable ignored) {
                    // Missing optional dependencies are expected in these JARs.
                }
            }
        }
    }
}
