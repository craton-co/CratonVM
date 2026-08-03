import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

/**
 * Prices the operation a Tomcat webapp deploy actually spends its time in.
 *
 * `--stack-dump-on-timeout` over a `TestManagerWebapp` deploy puts 94.6% of
 * samples (7806 of 8247) in
 *
 *   ContextConfig.processAnnotationsJar
 *     -> ... (JIT frames, not shown)
 *        -> java.io.BufferedInputStream.read([BII)I
 *
 * i.e. pulling every `.class` entry's bytes out of the webapp's JARs so BCEL
 * can scan them for annotations. This probe reads exactly that: every entry of
 * a real JAR, fully, through `JarFile.getInputStream`, and reports MiB/s. It
 * then splits the cost into raw `Inflater` throughput and `ZipFile` entry
 * overhead so the two can be told apart.
 *
 * Usage: JarEntryReadCostProbe <jar-or-dir> [more...]
 * With no argument it synthesises a JAR-shaped deflate workload so it still
 * reports something comparable on any machine.
 */
public class JarEntryReadCostProbe {

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("(no jar given — running the synthetic deflate workload only)");
        }
        for (String arg : args) {
            File f = new File(arg);
            if (f.isDirectory()) {
                File[] kids = f.listFiles((d, n) -> n.endsWith(".jar"));
                if (kids != null) {
                    for (File k : kids) {
                        timeJar(k);
                    }
                }
            } else if (f.isFile()) {
                timeJar(f);
            } else {
                System.out.println("skip (not found): " + arg);
            }
        }
        rawInflate();
    }

    private static void timeJar(File jar) throws IOException {
        // Two passes: the first also pays for opening/parsing the central
        // directory, the second is steady-state entry reading.
        for (int pass = 0; pass < 2; pass++) {
            long bytes = 0;
            int entries = 0;
            byte[] buf = new byte[8192];
            long t0 = System.nanoTime();
            try (JarFile jf = new JarFile(jar)) {
                Enumeration<JarEntry> en = jf.entries();
                while (en.hasMoreElements()) {
                    JarEntry e = en.nextElement();
                    if (e.isDirectory()) {
                        continue;
                    }
                    entries++;
                    try (InputStream in = jf.getInputStream(e)) {
                        int n;
                        while ((n = in.read(buf)) > 0) {
                            bytes += n;
                        }
                    }
                }
            }
            long dt = System.nanoTime() - t0;
            double mib = bytes / (1024.0 * 1024.0);
            System.out.printf("%-46s pass%d  %5d entries  %8.2f MiB  %8.1f ms  %8.2f MiB/s%n",
                    jar.getName(), pass, entries, mib, dt / 1e6, mib / (dt / 1e9));
        }
    }

    /**
     * Raw `Inflater` throughput on one 4 MiB deflated blob — no ZipFile, no
     * per-entry bookkeeping. Separates "inflate is slow" from "the ZipFile /
     * stream layering around it is slow".
     */
    private static void rawInflate() throws Exception {
        int size = 4 * 1024 * 1024;
        byte[] raw = new byte[size];
        // Class-file-ish: lots of repetition, so it actually compresses.
        for (int i = 0; i < size; i++) {
            raw[i] = (byte) ((i * 7) % 97);
        }
        Deflater def = new Deflater();
        def.setInput(raw);
        def.finish();
        byte[] compressed = new byte[size];
        int clen = 0;
        while (!def.finished() && clen < compressed.length) {
            clen += def.deflate(compressed, clen, compressed.length - clen);
        }
        def.end();

        byte[] out = new byte[size];
        double best = 0;
        for (int r = 0; r < 3; r++) {
            Inflater inf = new Inflater();
            inf.setInput(compressed, 0, clen);
            long t0 = System.nanoTime();
            int total = 0;
            while (!inf.finished() && total < out.length) {
                int n = inf.inflate(out, total, out.length - total);
                if (n == 0) {
                    break;
                }
                total += n;
            }
            long dt = System.nanoTime() - t0;
            inf.end();
            double mibs = (total / (1024.0 * 1024.0)) / (dt / 1e9);
            best = Math.max(best, mibs);
            if (total != size) {
                System.out.println("  (warning: inflated " + total + " of " + size + ")");
            }
        }
        System.out.printf("%-46s        %s  %8.2f MiB  %26.2f MiB/s%n", "raw Inflater.inflate (4 MiB blob)", "     ",
                size / (1024.0 * 1024.0), best);
    }
}
