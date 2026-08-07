import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

import org.h2.dev.fs.FilePathZip2;
import org.h2.mvstore.cache.FilePathCache;
import org.h2.store.fs.FilePath;
import org.h2.store.fs.FileUtils;
import org.h2.store.fs.encrypt.FilePathEncrypt;
import org.h2.store.fs.rec.FilePathRec;
import org.h2.test.TestBase;
import org.h2.test.utils.FilePathDebug;

/**
 * Drives org.h2.test.unit.TestFileSystem's per-filesystem body directly, one
 * prefix per argument, printing a BEGIN/OK/FAIL line with a wall-clock figure
 * for each. TestFileSystem.test() spends most of its ~800 s on the prefixes
 * before the one you care about, so running the class is a poor instrument for
 * any single filesystem; this reaches each one in seconds.
 *
 * The FilePath registrations below mirror the ones test() performs before its
 * later prefixes — without them "cache:", "rec:", "zip2:" and "encrypt:" do not
 * resolve.
 */
public class TfsProbe {

    public static void main(String... args) throws Exception {
        Class<?> c = Class.forName("org.h2.test.unit.TestFileSystem");
        TestBase t = (TestBase) c.getDeclaredConstructor().newInstance();
        t.init();
        Method body = c.getDeclaredMethod("testFileSystem", String.class);
        body.setAccessible(true);

        FilePathZip2.register();
        FilePath.register(new FilePathCache());
        FilePathRec.register();
        FilePathDebug.register().setTrace(false);
        FilePathEncrypt.register();

        String base = t.getBaseDir();
        String[] prefixes = args.length > 0 ? args
                : new String[] { base + "/fs", "memFS:", "nioMapped:" + base + "/fs" };

        int failed = 0;
        for (String raw : prefixes) {
            String p = raw.replace("@BASE@", base);
            long t0 = System.nanoTime();
            System.out.println("=== BEGIN " + p);
            System.out.flush();
            try {
                body.invoke(t, p);
                System.out.printf("=== OK    %s  %.1f s%n", p, (System.nanoTime() - t0) / 1e9);
            } catch (InvocationTargetException e) {
                failed++;
                System.out.printf("=== FAIL  %s  %.1f s%n", p, (System.nanoTime() - t0) / 1e9);
                e.getCause().printStackTrace(System.out);
            }
            System.out.flush();
        }
        FileUtils.delete(base + "/fs");
        System.out.println("=== DONE failed=" + failed);
        System.out.flush();
        System.exit(failed == 0 ? 0 : 1);
    }
}
