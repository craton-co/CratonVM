import java.io.File;
import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

import org.apache.tomcat.util.bcel.classfile.ClassParser;

/**
 * Which of two co-varying facts causes the second-loader step?
 *
 * `LoaderStepCostProbe` points every loader at the SAME jars, so from the
 * second loader onward two things change together: the process gains a second
 * user-defined loader, AND the same class NAMES acquire a second definition.
 * `loader-latch-degrades-every-deploy.md` attributes the surviving step to the
 * second fact -- `ClassManager::classify_exact_name` answering `Ambiguous`
 * instead of `Unique` -- but its probe cannot tell the two apart.
 *
 * This one separates them by giving each loader a DISJOINT jar:
 *
 *   +loader A (impl)   1 user loader, every name defined once
 *   +loader B (spec)   2 user loaders, every name STILL defined once
 *   +loader C (impl)   3 user loaders, impl's names now defined twice
 *
 * A step at B is about loader COUNT and has nothing to do with names. A step
 * that waits until C is name ambiguity, as the doc claims. Run with at least
 * two jars whose class names do not overlap.
 *
 * Reports process CPU time alongside wall time: this host is routinely
 * oversubscribed 2-3x, which swamps wall-clock deltas but leaves CPU time
 * usable. Each row is measured REPEAT times and every round is printed --
 * a minimum would hide exactly the monotone drift this is looking for.
 *
 * Usage: LoaderAmbiguityStepProbe &lt;dir-of-jars&gt; [repeat]
 */
public class LoaderAmbiguityStepProbe {

    private static final List<File> jars = new ArrayList<>();
    private static int repeat = 3;

    private static java.lang.management.OperatingSystemMXBean os =
        ManagementFactory.getOperatingSystemMXBean();

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("usage: LoaderAmbiguityStepProbe <dir-of-jars> [repeat]");
            return;
        }
        if (args.length > 1) {
            repeat = Integer.parseInt(args[1]);
        }
        File dir = new File(args[0]);
        File[] kids = dir.listFiles((d, n) -> n.endsWith(".jar"));
        if (kids == null || kids.length < 2) {
            System.out.println("need at least 2 jars under " + args[0]);
            return;
        }
        java.util.Arrays.sort(kids);
        for (File k : kids) {
            jars.add(k);
        }

        // Parse work is over ALL jars in every round, so the measured workload
        // never changes; only the loader/name state around it does.
        System.out.printf("%d jar(s)%n", jars.size());
        for (File jar : jars) {
            System.out.printf("  %s: %d classes%n", jar.getName(), classNames(jar).size());
        }
        System.out.printf("%-26s | %s%n", "after", "parse ms (wall / cpu) per round");

        report("baseline");

        // Disjoint loaders: A over jar 0, B over jar 1. B adds a second user
        // loader WITHOUT making any name ambiguous.
        List<ClassLoader> keepAlive = new ArrayList<>();
        keepAlive.add(defineAll("+loader A (" + jars.get(0).getName() + ")", jars.get(0)));
        keepAlive.add(defineAll("+loader B (" + jars.get(1).getName() + ")", jars.get(1)));
        // C re-defines jar 0's names under a third loader: ambiguity at last.
        keepAlive.add(defineAll("+loader C (" + jars.get(0).getName() + " AGAIN)", jars.get(0)));
        // D re-defines jar 1's names too, so every name is ambiguous.
        keepAlive.add(defineAll("+loader D (" + jars.get(1).getName() + " AGAIN)", jars.get(1)));

        System.out.println("loaders kept: " + keepAlive.size());
    }

    /** Load every class of one jar through a fresh loader, then re-measure. */
    private static ClassLoader defineAll(String label, File jar) throws Exception {
        URLClassLoader loader = new URLClassLoader(new URL[] {jar.toURI().toURL()}, null);
        int defined = 0;
        for (String n : classNames(jar)) {
            try {
                Class.forName(n, false, loader);
                defined++;
            }
            catch (Throwable ignored) {
                // optional dependencies -- the count printed is what landed
            }
        }
        report(label + " [" + defined + "]");
        return loader;
    }

    private static void report(String label) throws Exception {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < repeat; i++) {
            long w0 = System.nanoTime();
            long c0 = cpuNanos();
            parseAll();
            long wall = (System.nanoTime() - w0) / 1_000_000L;
            long cpu = (cpuNanos() - c0) / 1_000_000L;
            sb.append(String.format("%5d/%-5d", wall, cpu));
        }
        System.out.printf("%-26s | %s%n", label, sb);
    }

    /**
     * Process CPU time across all threads, so GC and JIT work is included.
     * Falls back to wall time if the com.sun extension is unavailable.
     */
    private static long cpuNanos() {
        if (os instanceof com.sun.management.OperatingSystemMXBean sun) {
            long t = sun.getProcessCpuTime();
            if (t >= 0) {
                return t;
            }
        }
        return System.nanoTime();
    }

    private static List<String> classNames(File jar) throws Exception {
        List<String> out = new ArrayList<>();
        try (JarFile jf = new JarFile(jar)) {
            Enumeration<JarEntry> en = jf.entries();
            while (en.hasMoreElements()) {
                String n = en.nextElement().getName();
                if (n.endsWith(".class") && !n.contains("module-info")) {
                    out.add(n.substring(0, n.length() - 6).replace('/', '.'));
                }
            }
        }
        return out;
    }

    /** The unchanging measured workload: BCEL-parse every class in every jar. */
    private static void parseAll() throws Exception {
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
                    }
                    catch (Exception ignored) {
                        // keep every round over the same set
                    }
                }
            }
        }
    }
}
