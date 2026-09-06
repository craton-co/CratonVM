import com.thoughtworks.qdox.JavaProjectBuilder;
import com.thoughtworks.qdox.model.JavaSource;
import java.io.StringReader;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * QdoxConcProbe3 <source-file> [threads] [iters] [warmup=0|1]
 *
 * Same as QdoxConcProbe, but `warmup=1` performs one full single-threaded
 * parse on the main thread BEFORE any worker starts. That drives
 * com.thoughtworks.qdox.parser.impl.Parser (and every class its parse
 * touches) all the way to Initialized, so the workers never race a first
 * use. If the failures survive warmup, the defect is not class-init
 * publication.
 */
public class QdoxConcProbe3 {
    public static void main(String[] args) throws Exception {
        String source = new String(Files.readAllBytes(Paths.get(args[0])));
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int itersPerThread = args.length > 2 ? Integer.parseInt(args[2]) : 2000;
        boolean warmup = args.length > 3 && Integer.parseInt(args[3]) != 0;
        if (warmup) {
            JavaProjectBuilder b = new JavaProjectBuilder();
            JavaSource js = b.addSource(new StringReader(source));
            System.out.println("warmup classes=" + js.getClasses().size()
                + " parserInit=" + com.thoughtworks.qdox.parser.impl.Parser.class.getName());
        }
        AtomicInteger ok = new AtomicInteger();
        AtomicInteger fail = new AtomicInteger();
        AtomicInteger printed = new AtomicInteger();
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
                        fail.incrementAndGet();
                        System.out.println("t" + tid + " iter " + i + " FAILED: " + e);
                        if (printed.incrementAndGet() <= 6) {
                            synchronized (QdoxConcProbe3.class) { e.printStackTrace(System.out); System.out.flush(); }
                        }
                    }
                }
            }, "w" + t);
        }
        long t0 = System.currentTimeMillis();
        for (Thread th : ts) th.start();
        for (Thread th : ts) th.join();
        System.out.println("DONE ok=" + ok.get() + " fail=" + fail.get() + " total=" + (threads*itersPerThread) + " ms=" + (System.currentTimeMillis()-t0));
    }
}
