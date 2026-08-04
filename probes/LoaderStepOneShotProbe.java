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
 * One loader state per PROCESS, so the cost can be read as CPU time from
 * outside instead of as wall-clock from inside.
 *
 * `LoaderStepCostProbe` measures every state in one process with
 * `System.nanoTime()`. On a shared, oversubscribed host that is unusable: a
 * 1.5x effect sits well inside the run-to-run spread, and the JIT warming up
 * across the run drifts the later rows in the opposite direction. Splitting
 * the states into separate processes lets the caller wrap each in
 * `/usr/bin/time` and compare USER CPU TIME, which measures work done rather
 * than time waited and barely moves when the host is busy.
 *
 * Every mode runs the identical parse workload; only the loader/name state
 * established before it differs.
 *
 *   none    parse only -- the floor
 *   empty   construct a URLClassLoader, define nothing
 *   one     define exactly ONE class through it
 *   all     define every class in the jars through it
 *   two     a second loader over the same jars: same NAMES defined twice
 *   three   a third
 *
 * Usage: LoaderStepOneShotProbe &lt;dir-of-jars&gt; &lt;mode&gt; [rounds]
 */
public class LoaderStepOneShotProbe {

    private static final List<File> jars = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.out.println("usage: LoaderStepOneShotProbe <dir-of-jars> <mode> [rounds]");
            return;
        }
        File dir = new File(args[0]);
        String mode = args[1];
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 6;

        File[] kids = dir.listFiles((d, n) -> n.endsWith(".jar"));
        if (kids == null || kids.length == 0) {
            System.out.println("no jars under " + args[0]);
            return;
        }
        java.util.Arrays.sort(kids);
        for (File k : kids) {
            jars.add(k);
        }
        URL[] urls = new URL[jars.size()];
        for (int i = 0; i < jars.size(); i++) {
            urls[i] = jars.get(i).toURI().toURL();
        }
        List<String> names = classNames();

        // Establish the loader state. Nothing here is measured -- the caller
        // times the whole process, and every mode's parse workload below is
        // identical, so the SETUP cost is the one thing that differs besides
        // the effect. Keep it small relative to `rounds` parse passes.
        List<ClassLoader> keepAlive = new ArrayList<>();
        int loaders = switch (mode) {
            case "none" -> 0;
            case "empty", "one", "all" -> 1;
            case "two" -> 2;
            case "three" -> 3;
            default -> throw new IllegalArgumentException("unknown mode " + mode);
        };
        for (int i = 0; i < loaders; i++) {
            URLClassLoader loader = new URLClassLoader(urls, null);
            keepAlive.add(loader);
            if (mode.equals("empty")) {
                continue;
            }
            if (mode.equals("one") && i == 0) {
                Class.forName(names.get(0), false, loader);
                continue;
            }
            for (String n : names) {
                try {
                    Class.forName(n, false, loader);
                }
                catch (Throwable ignored) {
                    // optional dependencies
                }
            }
        }

        // The measured workload, identical in every mode.
        for (int i = 0; i < rounds; i++) {
            parseAll();
        }
        System.out.println("mode=" + mode + " loaders=" + keepAlive.size() + " rounds=" + rounds);
    }

    private static List<String> classNames() throws Exception {
        List<String> out = new ArrayList<>();
        for (File jar : jars) {
            try (JarFile jf = new JarFile(jar)) {
                Enumeration<JarEntry> en = jf.entries();
                while (en.hasMoreElements()) {
                    String n = en.nextElement().getName();
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
                    }
                    catch (Exception ignored) {
                        // keep every round over the same set
                    }
                }
            }
        }
    }
}
