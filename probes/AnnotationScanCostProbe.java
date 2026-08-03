import java.io.BufferedInputStream;
import java.io.File;
import java.io.InputStream;
import java.util.Enumeration;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

import org.apache.tomcat.util.bcel.classfile.ClassParser;
import org.apache.tomcat.util.bcel.classfile.JavaClass;

/**
 * Standalone replica of the loop a Tomcat webapp deploy spends its time in, so
 * the deploy wall can be measured without starting Tomcat.
 *
 * `ContextConfig.processAnnotationsJar` walks every `.class` entry of a JAR and
 * hands its stream to `processAnnotationsStream`, which is exactly:
 *
 *     ClassParser parser = new ClassParser(is);
 *     JavaClass clazz = parser.parse();
 *
 * A `--stack-dump-on-timeout=75` sample of `TestManagerWebapp`'s deploy put
 * 7780 of 8248 frames (94.3%) in `java.io.BufferedInputStream.read([BII)I`
 * beneath `processAnnotationsJar`, with the rest in BCEL's `ConstantPool` /
 * `AnnotationEntry`. This probe times the two halves separately:
 *
 *   read-only : pull every `.class` entry's bytes and discard them
 *   parse     : the same walk, but BCEL-parse each entry (the real work)
 *
 * so "reading JARs is slow" and "parsing class files is slow" can be told
 * apart. Needs tomcat-util on the classpath (`apps/tomcat/.suite/cp.txt`).
 *
 * Usage: AnnotationScanCostProbe <jar-or-dir-of-jars> [more...]
 */
public class AnnotationScanCostProbe {

    private static final int ROUNDS = 3;

    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.out.println("usage: AnnotationScanCostProbe <jar-or-dir> [more...]");
            return;
        }
        for (String arg : args) {
            File f = new File(arg);
            if (f.isDirectory()) {
                File[] kids = f.listFiles((d, n) -> n.endsWith(".jar"));
                if (kids != null) {
                    java.util.Arrays.sort(kids);
                    for (File k : kids) {
                        run(k);
                    }
                }
            } else if (f.isFile()) {
                run(f);
            } else {
                System.out.println("skip (not found): " + arg);
            }
        }
    }

    private static void run(File jar) throws Exception {
        long readBest = Long.MAX_VALUE;
        long parseBest = Long.MAX_VALUE;
        int classes = 0;
        long bytes = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long[] readStats = new long[2];
            long t0 = System.nanoTime();
            walk(jar, false, readStats);
            readBest = Math.min(readBest, System.nanoTime() - t0);

            long[] parseStats = new long[2];
            t0 = System.nanoTime();
            walk(jar, true, parseStats);
            parseBest = Math.min(parseBest, System.nanoTime() - t0);
            classes = (int) parseStats[0];
            bytes = parseStats[1];
        }
        System.out.printf("%-46s %4d classes %7.0f KiB | read %8.1f ms | read+parse %8.1f ms | parse %8.1f ms (%6.1f us/class)%n",
                jar.getName(), classes, bytes / 1024.0, readBest / 1e6, parseBest / 1e6,
                (parseBest - readBest) / 1e6, (parseBest - readBest) / 1e3 / Math.max(1, classes));
    }

    /** `stats[0]` = class count, `stats[1]` = bytes read. */
    private static void walk(File jar, boolean parse, long[] stats) throws Exception {
        byte[] buf = new byte[8192];
        try (JarFile jf = new JarFile(jar)) {
            Enumeration<JarEntry> en = jf.entries();
            while (en.hasMoreElements()) {
                JarEntry e = en.nextElement();
                if (e.isDirectory() || !e.getName().endsWith(".class")) {
                    continue;
                }
                stats[0]++;
                try (InputStream in = new BufferedInputStream(jf.getInputStream(e))) {
                    if (parse) {
                        ClassParser parser = new ClassParser(in);
                        JavaClass clazz = parser.parse();
                        // Touch the result so nothing can be optimised away.
                        if (clazz.getClassName() == null) {
                            throw new IllegalStateException("null class name");
                        }
                    } else {
                        int n;
                        while ((n = in.read(buf)) > 0) {
                            stats[1] += n;
                        }
                    }
                }
            }
        }
    }
}
