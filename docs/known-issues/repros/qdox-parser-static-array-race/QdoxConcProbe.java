import com.thoughtworks.qdox.JavaProjectBuilder;
import com.thoughtworks.qdox.model.JavaSource;
import java.io.StringReader;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * N threads x M iterations, each with its OWN fresh JavaProjectBuilder
 * parsing the SAME source content. QDox's com.thoughtworks.qdox.parser.impl.Parser
 * carries two non-final static fields -- `static short[] yytable;` and
 * `static short[] yycheck;` -- lazily decoded on first use by static methods
 * of the same name (a classic unsynchronized lazy-singleton pattern with no
 * `volatile`/locking). This storms that first-use window from several
 * threads at once.
 *
 * Usage: QdoxConcProbe <source-file> [threads=8] [itersPerThread=2000]
 *
 * On CratonVM (gen/g1/zgc all tried): reliably produces
 * ArrayIndexOutOfBoundsException ("...for length 0") and, in the original
 * failure this reproduces, ClassCastException ("class TypeDef cannot be cast
 * to class TypeDef") inside Parser.yyparse -- within the first few hundred
 * iterations.
 *
 * On real HotSpot JDK 25, the SAME probe: 3 runs x 8 threads x 3000 iters
 * (72000 parses total) -- zero failures. See the writeup for what that
 * comparison does and does not prove.
 */
public class QdoxConcProbe {
    public static void main(String[] args) throws Exception {
        String source = new String(Files.readAllBytes(Paths.get(args[0])));
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int itersPerThread = args.length > 2 ? Integer.parseInt(args[2]) : 2000;
        AtomicInteger ok = new AtomicInteger();
        AtomicInteger fail = new AtomicInteger();
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < itersPerThread; i++) {
                    JavaProjectBuilder builder = new JavaProjectBuilder();
                    try {
                        JavaSource js = builder.addSource(new StringReader(source));
                        if (js.getClasses().size() == 1) ok.incrementAndGet();
                        else { fail.incrementAndGet(); System.out.println("t" + tid + " iter " + i + " unexpected class count"); }
                    } catch (Throwable e) {
                        int f = fail.incrementAndGet();
                        System.out.println("t" + tid + " iter " + i + " FAILED: " + e);
                        if (f <= 5) e.printStackTrace(System.out);
                    }
                }
            });
        }
        long t0 = System.currentTimeMillis();
        for (Thread th : ts) th.start();
        for (Thread th : ts) th.join();
        System.out.println("DONE ok=" + ok.get() + " fail=" + fail.get() + " total=" + (threads*itersPerThread) + " ms=" + (System.currentTimeMillis()-t0));
    }
}
