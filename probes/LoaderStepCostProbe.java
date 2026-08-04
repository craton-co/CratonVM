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
 * `DeployRepeatCostProbe` shows that class loading permanently slows down an
 * unrelated interpreted workload — BCEL-parsing class files — by ~1.8x, as a
 * STEP on the first load rather than a gradual drift, and identically with the
 * JIT disabled. This probe narrows what in "class loading" trips the step, by
 * timing the same parse loop between successively larger provocations:
 *
 *   baseline          parse only, three times, to establish the floor
 *   +empty loader     construct a URLClassLoader and load NOTHING from it
 *   +one class        load exactly one class through it
 *   +all classes      load every class in the JARs through it
 *   +second loader    do the whole thing again under a fresh loader, so the
 *                     same names now have two definitions
 *
 * Whichever line the parse column steps at is the trigger. "empty loader"
 * stepping means merely having a user-defined loader in existence is what
 * costs; "one class" means the first user-defined DEFINITION; "second loader"
 * means it is name AMBIGUITY (two loaders defining one name), not loading.
 *
 * Usage: LoaderStepCostProbe &lt;jar-or-dir-of-jars&gt;
 */
public class LoaderStepCostProbe {

    private static List<File> jars = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("usage: LoaderStepCostProbe <jar-or-dir>");
            return;
        }
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
        List<String> names = classNames();
        URL[] urls = new URL[jars.size()];
        for (int i = 0; i < jars.size(); i++) {
            urls[i] = jars.get(i).toURI().toURL();
        }
        System.out.printf("%d jar(s), %d classes%n", jars.size(), names.size());
        System.out.printf("%-24s | %9s%n", "after", "parse ms");

        report("baseline 1");
        report("baseline 2");
        report("baseline 3");

        URLClassLoader empty = new URLClassLoader(urls, null);
        report("+empty loader");

        Class.forName(names.get(0), false, empty);
        report("+one class");

        for (String n : names) {
            try {
                Class.forName(n, false, empty);
            } catch (Throwable ignored) {
                // optional dependencies
            }
        }
        report("+all classes");
        report("+all classes (again)");

        URLClassLoader second = new URLClassLoader(urls, null);
        for (String n : names) {
            try {
                Class.forName(n, false, second);
            } catch (Throwable ignored) {
                // optional dependencies
            }
        }
        report("+second loader");
        report("+second loader (again)");

        URLClassLoader third = new URLClassLoader(urls, null);
        for (String n : names) {
            try {
                Class.forName(n, false, third);
            } catch (Throwable ignored) {
                // optional dependencies
            }
        }
        report("+third loader");

        // Keep the loaders reachable so nothing is collected mid-measurement.
        System.out.println("loaders: " + (empty != null) + (second != null) + (third != null));
    }

    private static void report(String label) throws Exception {
        long t0 = System.nanoTime();
        parseAll();
        System.out.printf("%-24s | %9d%n", label, (System.nanoTime() - t0) / 1_000_000L);
    }

    private static List<String> classNames() throws Exception {
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
                    } catch (Exception ignored) {
                        // keep every round over the same set
                    }
                }
            }
        }
    }
}
