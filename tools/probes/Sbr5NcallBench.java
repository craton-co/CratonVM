// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.RandomAccessFile;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Properties;

/**
 * SBR round 5, lane {@code ncall}: ns/op of the generic native-call crossing. One row per run;
 * median of 7 timed repetitions (thread CPU time when the VM supports it, wall otherwise) after a
 * warm-up. {@code all} runs every row in one process.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr5NcallBench &lt;row|all&gt; [iterations]</pre>
 */
public final class Sbr5NcallBench {
    interface Op {
        void run(int n) throws Exception;
    }

    static volatile Object sink;
    static int plain(int x) { return x + 1; }

    static final String[] ROWS = {
        "empty", "plainStatic", "boolValueOf", "mathSin", "mathPow", "getProp", "arraycopy8", "arraycopy1k",
        "objHash", "idHash", "isInstance", "isArray", "curThread", "nanoTime", "strIntern", "fisRead1k", "fosWrite1k", "rafSeek", "rafLen", "rafRead1k", "rafWrite1k",
        "floatToBits", "longBits", "classGetName", "strHash"
    };

    static java.io.File fisFile, fosFile;
    static Op make(String row, RandomAccessFile raf, byte[] buf) throws Exception {
        if (fisFile == null) {
            fisFile = java.io.File.createTempFile("ncallfis", ".bin"); fisFile.deleteOnExit();
            try (java.io.FileOutputStream o = new java.io.FileOutputStream(fisFile)) { o.write(new byte[1 << 20]); }
            fosFile = java.io.File.createTempFile("ncallfos", ".bin"); fosFile.deleteOnExit();
        }
        switch (row) {
            case "empty": return k -> { for (int i = 0; i < k; i++) sink = null; };
            case "plainStatic": return k -> { int a = 0; for (int i = 0; i < k; i++) a = plain(a); sink = a; };
            case "boolValueOf": return k -> { int a = 0; for (int i = 0; i < k; i++) if (Boolean.valueOf((i & 1) == 0)) a++; sink = a; };
            case "mathSin": return k -> { double a = 0; for (int i = 0; i < k; i++) a += StrictMath.sin(i); sink = a; };
            case "mathPow": return k -> { double a = 0; for (int i = 0; i < k; i++) a += StrictMath.pow(i, 1.5); sink = a; };
            case "getProp": return k -> { int a = 0; for (int i = 0; i < k; i++) if (System.getProperty("java.version") != null) a++; sink = a; };
            case "arraycopy8": return k -> { int[] s = new int[8], d = new int[8]; for (int i = 0; i < k; i++) System.arraycopy(s, 0, d, 0, 8); sink = d; };
            case "arraycopy1k": return k -> { byte[] s = new byte[1024], d = new byte[1024]; for (int i = 0; i < k; i++) System.arraycopy(s, 0, d, 0, 1024); sink = d; };
            case "objHash": return k -> { Object o = new Object(); int a = 0; for (int i = 0; i < k; i++) a += o.hashCode(); sink = a; };
            case "idHash": return k -> { Object o = new Object(); int a = 0; for (int i = 0; i < k; i++) a += System.identityHashCode(o); sink = a; };
            case "isInstance": return k -> { Class<?> c = Number.class; Object o = Integer.valueOf(3); int a = 0; for (int i = 0; i < k; i++) if (c.isInstance(o)) a++; sink = a; };
            case "isArray": return k -> { Class<?> c = String[].class; int a = 0; for (int i = 0; i < k; i++) if (c.isArray()) a++; sink = a; };
            case "curThread": return k -> { int a = 0; for (int i = 0; i < k; i++) if (Thread.currentThread() != null) a++; sink = a; };
            case "nanoTime": return k -> { long a = 0; for (int i = 0; i < k; i++) a += System.nanoTime(); sink = a; };
            case "strIntern": return k -> { String s = new String("abcdef"); int a = 0; for (int i = 0; i < k; i++) if (s.intern() != null) a++; sink = a; };
            case "floatToBits": return k -> { int a = 0; for (int i = 0; i < k; i++) a += Float.floatToRawIntBits(i * 0.5f); sink = a; };
            case "longBits": return k -> { long a = 0; for (int i = 0; i < k; i++) a += Double.doubleToRawLongBits(i * 0.5); sink = a; };
            case "classGetName": return k -> { Class<?> c = String.class; int a = 0; for (int i = 0; i < k; i++) if (c.getName() != null) a++; sink = a; };
            case "strHash": return k -> { int a = 0; String s = "hello"; for (int i = 0; i < k; i++) a += s.hashCode(); sink = a; };
            case "fisRead1k": return k -> { java.io.FileInputStream fis = new java.io.FileInputStream(fisFile); int c = 0; for (int i = 0; i < k; i++) { int n = fis.read(buf, 0, 1024); if (n < 0) { fis.getChannel().position(0); } c += n; } fis.close(); sink = c; };
            case "fosWrite1k": return k -> { java.io.FileOutputStream fos = new java.io.FileOutputStream(fosFile); for (int i = 0; i < k; i++) { fos.write(buf, 0, 1024); if ((i & 4095) == 4095) fos.getChannel().truncate(0); } fos.close(); };
            case "rafSeek": return k -> { for (int i = 0; i < k; i++) { raf.seek(0); } };
            case "rafLen": return k -> { long a = 0; for (int i = 0; i < k; i++) { a += raf.length(); } sink = a; };
            case "rafRead1k": return k -> { for (int i = 0; i < k; i++) { raf.seek(0); raf.readFully(buf, 0, 1024); } };
            case "rafWrite1k": return k -> { for (int i = 0; i < k; i++) { raf.seek(0); raf.write(buf, 0, 1024); } };
            default: throw new IllegalArgumentException(row);
        }
    }

    static long now(ThreadMXBean mx) {
        return mx != null ? mx.getCurrentThreadCpuTime() : System.nanoTime();
    }

    static void time(String row, int n, RandomAccessFile raf, byte[] buf, ThreadMXBean mx) throws Exception {
        Op op = make(row, raf, buf);
        op.run(Math.min(n, 100000));
        op.run(n);
        long[] t = new long[7];
        for (int rep = 0; rep < t.length; rep++) {
            long t0 = now(mx);
            op.run(n);
            t[rep] = now(mx) - t0;
        }
        java.util.Arrays.sort(t);
        System.out.println(row + " " + (t[3] / n) + " ns/op (min " + (t[0] / n) + ")");
    }

    public static void main(String[] args) throws Exception {
        String row = args.length > 0 ? args[0] : "all";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200000;
        ThreadMXBean mx = null;
        try {
            ThreadMXBean m = ManagementFactory.getThreadMXBean();
            if (m.isCurrentThreadCpuTimeSupported() && m.getCurrentThreadCpuTime() > 0) mx = m;
        } catch (Throwable ignore) {
        }
        java.io.File f = java.io.File.createTempFile("ncall", ".bin");
        f.deleteOnExit();
        byte[] buf = new byte[4096];
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            raf.write(buf);
            if (row.equals("all")) {
                for (String r : ROWS) time(r, r.startsWith("raf") ? n / 4 : n, raf, buf, mx);
            } else {
                time(row, n, raf, buf, mx);
            }
        }
        System.out.println("clock=" + (mx != null ? "cpu" : "wall"));
    }
}
