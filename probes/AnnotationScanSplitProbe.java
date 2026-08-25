import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.File;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

import org.apache.tomcat.util.bcel.classfile.ClassParser;

/**
 * Splits the Tomcat webapp-deploy annotation scan into the parts it is made
 * of, to settle which one the 226x actually is.
 *
 * `docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`
 * attributes the cost to object construction ("the parse is never compiled"),
 * while a `--stack-dump-on-timeout` of the same workload puts 94% of samples
 * in `BufferedInputStream.read`. Those are different claims with different
 * fixes. Running all five stages over the SAME in-memory class bytes decides
 * it — and on CratonVM `readBytes` alone is ~99% of `parseMem`, so the cost is
 * the per-byte I/O call chain, not construction and not class resolution.
 *
 *   parseMem  : ClassParser.parse() — object construction + the I/O chain
 *   readBytes : DataInputStream.readUnsignedByte() per byte, constructing
 *               nothing — the I/O call chain alone
 *   readRaw   : ByteArrayInputStream.read() per byte — one layer less
 *   arrayRead : b[i] — the floor, no calls at all
 *   allocOnly : one tiny object per byte — construction alone
 *
 * Every stage loop is inlined into a NAMED STATIC METHOD rather than a
 * functional-interface body, deliberately: the retired tomcat/30 write-up
 * records that driving stages through a `Runnable`/`IntConsumer` made every
 * row read ~3.4 us, because that is what a lambda interface call costs here —
 * it buries the thing being compared. The first draft of this probe repeated
 * that mistake. If you add a stage, add it as a static method.
 *
 * This probe WAS also the reproduction for a JIT miscompile: it SIGSEGVd in
 * `arrayRead` on the real Tomcat classpath (Windows), and on Linux the same
 * defect silently summed the wrong bytes without failing anything, because
 * `sink` is discarded here. Fixed 2026-08-04 by `14a274085` — a slot javac
 * reuses as both a live reference and a `long`'s high half must keep its OSR
 * register home. `probes/OsrRefSlotReuseProbe.java` is the minimised,
 * self-checking version and is the regression guard; see the retired
 * `annotation-scan-arrayread-sigsegv` write-up.
 */
public class AnnotationScanSplitProbe {

    static final class Small {
        final int v;
        Small(int v) { this.v = v; }
    }

    private static List<byte[]> classes = new ArrayList<>();
    private static long totalBytes;

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("usage: AnnotationScanSplitProbe <jar-or-dir> [rounds]");
            return;
        }
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        List<File> jars = new ArrayList<>();
        File f = new File(args[0]);
        if (f.isDirectory()) {
            File[] kids = f.listFiles((d, n) -> n.endsWith(".jar"));
            if (kids != null) { for (File k : kids) { jars.add(k); } }
        } else if (f.isFile()) {
            jars.add(f);
        }
        for (File jar : jars) {
            try (JarFile jf = new JarFile(jar)) {
                Enumeration<JarEntry> en = jf.entries();
                while (en.hasMoreElements()) {
                    JarEntry e = en.nextElement();
                    if (!e.getName().endsWith(".class")) { continue; }
                    try (InputStream is = jf.getInputStream(e)) {
                        classes.add(is.readAllBytes());
                    }
                }
            }
        }
        for (byte[] b : classes) { totalBytes += b.length; }
        System.out.printf("%d classes, %d KiB%n", classes.size(), totalBytes / 1024);
        System.out.printf("%-10s | %8s | %9s%n", "stage", "ms", "ns/byte");

        for (int r = 1; r <= rounds; r++) {
            System.out.println("-- round " + r);
            report("parseMem", parseMem());
            report("readBytes", readBytes());
            report("readRaw", readRaw());
            report("arrayRead", arrayRead());
            report("allocOnly", allocOnly());
        }
    }

    private static void report(String label, long ns) {
        System.out.printf("%-10s | %8d | %9.1f%n", label, ns / 1_000_000L, (double) ns / totalBytes);
    }

    private static long parseMem() {
        long t0 = System.nanoTime();
        for (byte[] b : classes) {
            try {
                new ClassParser(new ByteArrayInputStream(b)).parse();
            } catch (Exception ignored) { /* keep the set constant */ }
        }
        return System.nanoTime() - t0;
    }

    private static long readBytes() throws Exception {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : classes) {
            DataInputStream in = new DataInputStream(new ByteArrayInputStream(b));
            for (int i = 0; i < b.length; i++) { sink += in.readUnsignedByte(); }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) { System.out.print(""); }
        return ns;
    }

    private static long readRaw() {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : classes) {
            ByteArrayInputStream in = new ByteArrayInputStream(b);
            for (int i = 0; i < b.length; i++) { sink += in.read(); }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) { System.out.print(""); }
        return ns;
    }

    private static long arrayRead() {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : classes) {
            for (int i = 0; i < b.length; i++) { sink += b[i]; }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) { System.out.print(""); }
        return ns;
    }

    private static long allocOnly() {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : classes) {
            for (int i = 0; i < b.length; i++) { sink += new Small(b[i]).v; }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) { System.out.print(""); }
        return ns;
    }
}
