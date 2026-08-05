import java.io.BufferedInputStream;
import java.io.FileInputStream;
import java.util.jar.JarEntry;
import java.util.jar.JarInputStream;

/**
 * Reads a jar entry-by-entry, repeatedly, and checks every iteration against the first.
 *
 * <p>Written for the loader/zip JIT cluster
 * ({@code docs/known-issues/springboot/loader-zip-jit-only-failure-cluster-20260804.md}).
 * The defect was that {@code jdk.internal.misc.Unsafe.ARRAY_BYTE_BASE_OFFSET} — a
 * {@code long} in JDK 25 — was stored as a {@code Value::Int}, which the interpreter
 * widened silently while a JIT-compiled {@code getstatic …:J} read the adjacent word.
 * {@code java.util.zip.ZipUtils.get16/get32} address every LOC/CEN header field through
 * that offset, so every size and method field parsed out of a jar was garbage.
 *
 * <p>What makes this probe worth keeping is the FIRST iteration: before it ever threw,
 * the broken build silently read 99 of the jar's 120 entries. Only comparing the entry
 * and byte counts catches that — an exception-only check reports success on a jar loader
 * that has lost a fifth of its contents.
 *
 * <pre>
 *   javac -d . probes/ZipSpin.java
 *   java     -cp . ZipSpin &lt;some.jar&gt; 200      # expect: OK …
 *   cratonvm -cp . ZipSpin &lt;some.jar&gt; 200      # must print the SAME entries/bytes
 * </pre>
 *
 * Compare the two lines. Any difference in {@code entries=} or {@code bytes=} is the bug,
 * with or without an exception. {@code --nojit} passing while the default run does not is
 * the cluster's signature.
 */
public class ZipSpin {
    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: ZipSpin <jar> [iterations]");
            return;
        }
        String path = args[0];
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        long firstTotal = -1;
        int firstEntries = -1;
        for (int i = 0; i < iters; i++) {
            long total = 0;
            int entries = 0;
            try (JarInputStream in = new JarInputStream(new BufferedInputStream(new FileInputStream(path)))) {
                JarEntry e;
                byte[] buf = new byte[8192];
                while ((e = in.getNextJarEntry()) != null) {
                    entries++;
                    int r;
                    while ((r = in.read(buf)) != -1) {
                        total += r;
                    }
                }
            } catch (Throwable t) {
                System.out.println("ITER " + i + " THREW after " + entries + " entries / " + total
                        + " bytes: " + t);
                return;
            }
            if (i == 0) {
                firstTotal = total;
                firstEntries = entries;
                System.out.println("iter0 entries=" + entries + " bytes=" + total);
            } else if (total != firstTotal || entries != firstEntries) {
                System.out.println("ITER " + i + " DIVERGED: entries=" + entries + " (want "
                        + firstEntries + ") bytes=" + total + " (want " + firstTotal + ")");
                return;
            }
        }
        System.out.println("OK " + iters + " iterations, entries=" + firstEntries
                + " bytes=" + firstTotal);
    }
}
