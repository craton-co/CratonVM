// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import javax.management.MBeanServer;
import javax.management.ObjectName;
import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;

/**
 * gci i41/rs ({@code gci-i39rv-waves-i35-i38-review-residuals-20261016} item 2):
 * a HotSpot-side census of WHERE an interface's / an abstract class's Klass
 * lives. gci i36/ms put every Klass in the model's class space
 * ({@code vm_init.rs} {@code HsKlassShapes::class_parts}) on the evidence that
 * HotSpot 25 has no JDK-8338526 (which moves interface and abstract Klasses out
 * of the class space). This probe measures it: it generates {@code n} (default
 * 1000) interfaces {@code I0..}, abstract classes {@code A0..} and concrete
 * classes {@code C0..} of one shape (one {@code int m(int)} method), compiles
 * them with the platform compiler ({@code javax.tools}), and loads each batch
 * through its own class loader (not initialised), reading the
 * {@code Compressed Class Space} and {@code Metaspace} pools' used bytes, and
 * the {@code Class space} lines of {@code VM.metaspace} (the
 * {@code DiagnosticCommand} MBean, i.e. {@code jcmd <pid> VM.metaspace}),
 * before and after each batch.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp &lt;classes&gt; GciI41RsKlassSpaceCensusProbe [n]
 * </pre>
 *
 * stderr (the census; judged on HotSpot's {@code .hse} only):
 * {@code [i41rs-ms] batch=<I|A|C> n=<n> ccs_delta=<bytes> meta_delta=<bytes>
 * ccs_per_class=<bytes>}, one {@code [i41rs-ms] vm.metaspace <edge> <line>} per
 * {@code Class space} / {@code Class:} line of {@code VM.metaspace} at each edge,
 * and {@code [i41rs-ms] ratio I/C=<r> A/C=<r>} (class-space deltas, 3 decimals).
 * stdout: {@code klass space census done} (or {@code no compiler}).
 */
public class GciI41RsKlassSpaceCensusProbe {
    static final List<Object> keep = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1000;
        JavaCompiler javac = ToolProvider.getSystemJavaCompiler();
        if (javac == null) {
            System.out.println("no compiler");
            return;
        }
        Path root = Files.createTempDirectory("i41rs-ms");
        Path warm = batch(javac, root, "W", 1, "public class %s { public int m(int x) { return x; } }");
        Path iface = batch(javac, root, "I", n, "public interface %s { int m(int x); }");
        Path abs = batch(javac, root, "A", n, "public abstract class %s { public abstract int m(int x); }");
        Path conc = batch(javac, root, "C", n, "public class %s { public int m(int x) { return x; } }");
        // Warm the loading path (URLClassLoader, the jar/file readers) through a
        // loader of its own, so the first measured batch pays for nothing else.
        load(warm, "W", 1);
        System.gc();
        long[] e0 = edge("start");
        load(iface, "I", n);
        long[] e1 = edge("after-I");
        load(abs, "A", n);
        long[] e2 = edge("after-A");
        load(conc, "C", n);
        long[] e3 = edge("after-C");
        long dI = e1[0] - e0[0];
        long dA = e2[0] - e1[0];
        long dC = e3[0] - e2[0];
        report("I", n, dI, e1[1] - e0[1]);
        report("A", n, dA, e2[1] - e1[1]);
        report("C", n, dC, e3[1] - e2[1]);
        System.err.println("[i41rs-ms] ratio I/C=" + ratio(dI, dC) + " A/C=" + ratio(dA, dC));
        System.out.println("klass space census done" + (keep.size() == 3 * n + 5 ? "" : "!"));
    }

    static String ratio(long a, long b) {
        if (b <= 0) {
            return "none";
        }
        return String.format("%.3f", (double) a / (double) b);
    }

    static void report(String batch, int n, long ccs, long meta) {
        System.err.println("[i41rs-ms] batch=" + batch + " n=" + n + " ccs_delta=" + ccs
                + " meta_delta=" + meta + " ccs_per_class=" + (ccs / n));
    }

    /** Writes {@code n} sources {@code <prefix>0..} and compiles them into {@code <root>/<prefix>}. */
    static Path batch(JavaCompiler javac, Path root, String prefix, int n, String shape) throws IOException {
        Path src = root.resolve(prefix + "-src");
        Path out = root.resolve(prefix);
        Files.createDirectories(src);
        Files.createDirectories(out);
        List<String> argv = new ArrayList<>();
        argv.add("-d");
        argv.add(out.toString());
        argv.add("-nowarn");
        for (int i = 0; i < n; i++) {
            String name = prefix + i;
            Path f = src.resolve(name + ".java");
            Files.write(f, String.format(shape, name).getBytes(StandardCharsets.UTF_8));
            argv.add(f.toString());
        }
        int rc = javac.run(null, null, null, argv.toArray(new String[0]));
        if (rc != 0) {
            throw new IllegalStateException("javac rc=" + rc + " for batch " + prefix);
        }
        return out;
    }

    static void load(Path dir, String prefix, int n) throws Exception {
        URLClassLoader loader = new URLClassLoader(new URL[] {dir.toUri().toURL()}, null);
        keep.add(loader);
        for (int i = 0; i < n; i++) {
            keep.add(Class.forName(prefix + i, false, loader));
        }
    }

    /** {Compressed Class Space used, Metaspace used}, plus the VM.metaspace class lines on stderr. */
    static long[] edge(String label) {
        long ccs = -1;
        long meta = -1;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals("Compressed Class Space")) {
                ccs = p.getUsage().getUsed();
            } else if (p.getName().equals("Metaspace")) {
                meta = p.getUsage().getUsed();
            }
        }
        System.err.println("[i41rs-ms] pools " + label + " ccs_used=" + ccs + " meta_used=" + meta);
        try {
            MBeanServer server = ManagementFactory.getPlatformMBeanServer();
            ObjectName dcmd = new ObjectName("com.sun.management:type=DiagnosticCommand");
            Object text = server.invoke(dcmd, "vmMetaspace", new Object[] {new String[0]},
                    new String[] {String[].class.getName()});
            boolean inTotal = false;
            for (String line : String.valueOf(text).split("\n")) {
                String t = line.trim();
                if (t.startsWith("Total Usage")) {
                    inTotal = true;
                }
                if (t.startsWith("Class space") || (inTotal && t.startsWith("Class:"))) {
                    System.err.println("[i41rs-ms] vm.metaspace " + label + " " + t);
                    if (t.startsWith("Class:")) {
                        inTotal = false;
                    }
                }
            }
        } catch (Exception e) {
            System.err.println("[i41rs-ms] vm.metaspace " + label + " unavailable " + e);
        }
        return new long[] {ccs, meta};
    }
}
