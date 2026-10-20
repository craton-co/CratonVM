// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.OutputStream;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.CRC32;
import javax.tools.Diagnostic;
import javax.tools.DiagnosticCollector;
import javax.tools.FileObject;
import javax.tools.ForwardingJavaFileManager;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.SimpleJavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

/**
 * gci i1/pn (2026-10-11): the JDK's own compiler over a real source tree, in
 * process, on several threads, as a battery row.
 *
 * <p>The deciding workload of the pinned young copy's flip gate
 * ({@code docs/known-issues/gc/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md},
 * gate R1). gch h4/bp wrote R1 as a MANUAL host loop that diffs class-file
 * trees; this probe makes the same check readable by the battery's
 * byte-for-byte stdout comparison with HotSpot: every class file javac emits
 * is captured in memory and folded into a CRC, so a single corrupted byte of
 * compiler output, a lost object or a wrong branch in javac's own code changes
 * the printed digest.
 *
 * <p>Shape. {@code files} source files are sampled from {@code srcDir} (every
 * {@code *.java} directly in it, sorted by name, taken at an even stride).
 * Each round compiles every sampled file in its own javac task, on
 * {@code threads} worker threads that pull file indices from a shared counter
 * (results are stored by index, so the thread schedule cannot change the
 * output). The classpath and sourcepath name a directory that does not exist,
 * so a file that needs another probe's classes fails with errors, the same
 * way on every JVM; only the counts are printed, never the messages. Each
 * round's class bytes are kept until the end of the NEXT round and then
 * re-checked against their CRC ({@code corrupt=}): medium-lived data that a
 * young collection must carry, promote or pin while javac churns.
 *
 * <p>javac is allocation-heavy, JIT-warm after the first round, and on
 * {@code threads > 1} its mutators are parked in compiled code at every pause:
 * the shape of the real workloads (netty, the Tomcat fixture) that are not
 * available on the probe host.
 *
 * <p>stdout (deterministic; HotSpot 25 {@code -XX:+UseSerialGC} is the
 * oracle; the same JDK must serve both runs, since the digest is javac's
 * output): a header line, one line per round, then
 * {@code PASS javac-tree files=F rounds=R threads=T stable=true corrupt=0 digest=...}.
 * Timings go to stderr.
 *
 * <p>Run: {@code cratonvm -XX:+UseGenerationalGC -Xmx128m -cp <classes>
 * GciPn1JavacTreeProbe tools/probes 96 3 4}.
 */
public class GciPn1JavacTreeProbe {
    /** A directory that exists nowhere: the classpath and the sourcepath. */
    static final String NOWHERE = "gci-pn1-no-such-dir";

    static final class Src extends SimpleJavaFileObject {
        final String code;

        Src(String fileName, String code) {
            super(URI.create("string:///" + fileName), Kind.SOURCE);
            this.code = code;
        }

        @Override
        public CharSequence getCharContent(boolean ignoreEncodingErrors) {
            return code;
        }
    }

    static final class ClassOut extends SimpleJavaFileObject {
        final ByteArrayOutputStream bytes = new ByteArrayOutputStream();

        ClassOut(String className) {
            super(URI.create("mem:///" + className.replace('.', '/') + ".class"), Kind.CLASS);
        }

        @Override
        public OutputStream openOutputStream() {
            return bytes;
        }
    }

    static final class MemFm extends ForwardingJavaFileManager<StandardJavaFileManager> {
        final TreeMap<String, ClassOut> out = new TreeMap<>();

        MemFm(StandardJavaFileManager fm) {
            super(fm);
        }

        @Override
        public JavaFileObject getJavaFileForOutput(
                Location location, String className, JavaFileObject.Kind kind, FileObject sibling) {
            ClassOut c = new ClassOut(className);
            out.put(className, c);
            return c;
        }
    }

    /** One file's result in one round. */
    static final class Result {
        long crc;
        int errors;
        int classes;
        boolean threw;
        byte[][] kept = new byte[0][];
        long[] keptCrc = new long[0];
    }

    static long crcOf(byte[] b) {
        CRC32 c = new CRC32();
        c.update(b, 0, b.length);
        return c.getValue();
    }

    static Result compileOne(JavaCompiler jc, MemFm fm, String fileName, String code) {
        Result r = new Result();
        fm.out.clear();
        DiagnosticCollector<JavaFileObject> diags = new DiagnosticCollector<>();
        List<String> opts = Arrays.asList(
                "-proc:none", "-implicit:none", "-g", "-nowarn", "-Xlint:none",
                "-classpath", NOWHERE, "-sourcepath", NOWHERE);
        try {
            Boolean ok = jc.getTask(null, fm, diags, opts, null, List.of(new Src(fileName, code))).call();
            for (Diagnostic<? extends JavaFileObject> d : diags.getDiagnostics()) {
                if (d.getKind() == Diagnostic.Kind.ERROR) {
                    r.errors++;
                }
            }
            if (!Boolean.TRUE.equals(ok) && r.errors == 0) {
                r.errors = 1;
            }
        } catch (Throwable t) {
            r.threw = true;
            System.err.println("[javac-tree] " + fileName + " threw " + t);
        }
        CRC32 all = new CRC32();
        r.kept = new byte[fm.out.size()][];
        r.keptCrc = new long[fm.out.size()];
        int i = 0;
        for (Map.Entry<String, ClassOut> e : fm.out.entrySet()) {
            byte[] name = e.getKey().getBytes(StandardCharsets.UTF_8);
            byte[] body = e.getValue().bytes.toByteArray();
            all.update(name, 0, name.length);
            all.update(body, 0, body.length);
            r.kept[i] = body;
            r.keptCrc[i] = crcOf(body);
            i++;
        }
        r.classes = i;
        r.crc = all.getValue();
        fm.out.clear();
        return r;
    }

    public static void main(String[] args) throws Exception {
        String dir = args.length > 0 ? args[0] : "tools/probes";
        int want = args.length > 1 ? Integer.parseInt(args[1]) : 96;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        int threads = args.length > 3 ? Integer.parseInt(args[3]) : 4;

        File[] listed = new File(dir).listFiles((d, n) -> n.endsWith(".java"));
        if (listed == null || listed.length == 0) {
            System.out.println("FAIL javac-tree no sources in " + dir);
            System.exit(1);
        }
        String[] names = new String[listed.length];
        for (int i = 0; i < listed.length; i++) {
            names[i] = listed[i].getName();
        }
        Arrays.sort(names);
        int n = Math.min(want, names.length);
        String[] files = new String[n];
        String[] codes = new String[n];
        for (int i = 0; i < n; i++) {
            files[i] = names[(int) ((long) i * names.length / n)];
            codes[i] = new String(Files.readAllBytes(new File(dir, files[i]).toPath()), StandardCharsets.UTF_8);
        }
        System.out.println("javac-tree files=" + n + " rounds=" + rounds + " threads=" + threads);

        JavaCompiler jc = ToolProvider.getSystemJavaCompiler();
        if (jc == null) {
            System.out.println("FAIL javac-tree no system compiler");
            System.exit(1);
        }

        Result[] previous = null;
        long firstDigest = 0;
        boolean stable = true;
        long corrupt = 0;
        for (int round = 1; round <= rounds; round++) {
            long t0 = System.nanoTime();
            Result[] results = new Result[n];
            AtomicInteger next = new AtomicInteger();
            List<Thread> workers = new ArrayList<>();
            Throwable[] failure = new Throwable[1];
            for (int t = 0; t < threads; t++) {
                Thread w = new Thread(() -> {
                    try {
                        StandardJavaFileManager sfm =
                                jc.getStandardFileManager(null, Locale.ROOT, StandardCharsets.UTF_8);
                        MemFm fm = new MemFm(sfm);
                        for (int i = next.getAndIncrement(); i < n; i = next.getAndIncrement()) {
                            results[i] = compileOne(jc, fm, files[i], codes[i]);
                        }
                        fm.close();
                    } catch (Throwable e) {
                        synchronized (failure) {
                            failure[0] = e;
                        }
                    }
                }, "javac-tree-" + t);
                workers.add(w);
                w.start();
            }
            for (Thread w : workers) {
                w.join();
            }
            if (failure[0] != null) {
                System.out.println("FAIL javac-tree round " + round + " worker threw " + failure[0].getClass().getName());
                failure[0].printStackTrace();
                System.exit(1);
            }
            // The previous round's class bytes lived through this whole round.
            if (previous != null) {
                for (Result p : previous) {
                    for (int k = 0; k < p.kept.length; k++) {
                        if (crcOf(p.kept[k]) != p.keptCrc[k]) {
                            corrupt++;
                        }
                    }
                }
            }
            long digest = 17;
            int ok = 0;
            int errors = 0;
            int classes = 0;
            int threw = 0;
            for (Result r : results) {
                digest = digest * 31 + r.crc;
                if (r.errors == 0 && !r.threw) {
                    ok++;
                }
                errors += r.errors;
                classes += r.classes;
                if (r.threw) {
                    threw++;
                }
            }
            if (round == 1) {
                firstDigest = digest;
            } else if (digest != firstDigest) {
                stable = false;
            }
            System.out.println("round " + round + " ok=" + ok + " errors=" + errors + " classes=" + classes
                    + " threw=" + threw + " digest=" + Long.toHexString(digest));
            System.err.println("[javac-tree] round " + round + " ms=" + (System.nanoTime() - t0) / 1_000_000);
            previous = results;
        }
        String line = "javac-tree files=" + n + " rounds=" + rounds + " threads=" + threads
                + " stable=" + stable + " corrupt=" + corrupt + " digest=" + Long.toHexString(firstDigest);
        System.out.println((stable && corrupt == 0 ? "PASS " : "FAIL ") + line);
    }
}
