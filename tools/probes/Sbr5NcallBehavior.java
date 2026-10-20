// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.util.ArrayList;
import java.util.List;

/**
 * SBR round 5, lane {@code ncall}: behaviour of natives reached from compiled code through the lean
 * native door and the leaf {@code RandomAccessFile} rows. Every line is deterministic and is diffed
 * against HotSpot; the timing line ({@code speed:}) is informational and excluded from the diff.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr5NcallBehavior | grep -v '^speed:'</pre>
 */
public final class Sbr5NcallBehavior {
    static volatile Object sink;
    static final int HOT = 30000;

    static void row(String name, Object v) {
        System.out.println(name + " " + v);
    }

    interface Shape { int h(); }
    static final class A implements Shape { public int h() { return System.identityHashCode(this) == 0 ? 1 : 2; } }
    static final class B implements Shape { public int h() { return 3; } }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static int idStable(Object o) {
        int ok = 0;
        int first = System.identityHashCode(o);
        for (int i = 0; i < HOT; i++) if (System.identityHashCode(o) == first) ok++;
        return ok;
    }

    static int hashStable(Object o) {
        int ok = 0;
        int first = o.hashCode();
        for (int i = 0; i < HOT; i++) if (o.hashCode() == first) ok++;
        return ok;
    }

    static int polyHash(Object[] objs) {
        int acc = 0;
        for (int i = 0; i < HOT; i++) {
            Object o = objs[i % objs.length];
            acc += (o.hashCode() == o.hashCode()) ? 1 : 0;
        }
        return acc;
    }

    static String nullReceiver(Object o) {
        try {
            int a = 0;
            for (int i = 0; i < HOT; i++) a += o.hashCode() == 0 ? 0 : 1;
            return "no-npe " + a;
        } catch (NullPointerException e) {
            return "npe";
        }
    }

    static String nullArg() {
        try {
            long a = 0;
            byte[] src = null;
            for (int i = 0; i < HOT; i++) { System.arraycopy(src, 0, new byte[4], 0, 1); a++; }
            return "no-npe " + a;
        } catch (NullPointerException e) {
            return "npe";
        }
    }

    static String arrayStore() {
        try {
            for (int i = 0; i < HOT; i++) System.arraycopy(new int[2], 0, new int[2], 0, 3);
            return "no-throw";
        } catch (ArrayIndexOutOfBoundsException e) {
            return ex(e);
        }
    }

    public static void main(String[] args) throws Exception {
        File f = File.createTempFile("ncallb", ".bin");
        f.deleteOnExit();
        byte[] data = new byte[4096];
        for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 31 + 7);

        // ---- RandomAccessFile data path, hot enough to compile every wrapper ----
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            for (int i = 0; i < 200; i++) { raf.seek(0); raf.write(data, 0, 1024); }
            row("raf.length", raf.length());
            long sum = 0;
            byte[] buf = new byte[1024];
            for (int i = 0; i < HOT; i++) {
                raf.seek((i % 3) * 1024L % 1024);
                raf.readFully(buf, 0, 512);
                sum += buf[i & 511];
            }
            row("raf.readsum", sum);
            raf.seek(0);
            raf.write(data, 0, 4096);
            row("raf.length2", raf.length());
            raf.seek(4090);
            byte[] tail = new byte[100];
            row("raf.read-tail", raf.read(tail, 0, 100));
            row("raf.read-eof", raf.read(tail, 0, 100));
            row("raf.fp", raf.getFilePointer());
            row("raf.read1-eof", raf.read());
            raf.seek(10);
            row("raf.read1", raf.read());
            row("raf.fp2", raf.getFilePointer());
            raf.write(0x55);
            raf.seek(10);
            row("raf.write1", raf.read());
            try { raf.seek(-1); } catch (IOException e) { row("raf.seek-neg", ex(e)); }
            try { raf.read(buf, 0, 2000); row("raf.read-oob", "no-throw"); }
            catch (IndexOutOfBoundsException e) { row("raf.read-oob", ex(e)); }
            try { raf.write(buf, 1000, 100); row("raf.write-oob", "no-throw"); }
            catch (IndexOutOfBoundsException e) { row("raf.write-oob", ex(e)); }
            try { raf.read(buf, -1, 10); row("raf.read-negoff", "no-throw"); }
            catch (IndexOutOfBoundsException e) { row("raf.read-negoff", ex(e)); }
            row("raf.read-zero", raf.read(buf, 0, 0));
            raf.seek(0);
            byte[] back = new byte[4096];
            raf.readFully(back);
            int same = 0;
            for (int i = 0; i < back.length; i++) if (back[i] == data[i]) same++;
            row("raf.roundtrip-same", same);
            raf.setLength(100);
            row("raf.setlen", raf.length());
            // compiled-loop failure after close
            raf.close();
            String closed = "no-throw";
            try {
                for (int i = 0; i < HOT; i++) { raf.seek(0); raf.read(); }
            } catch (IOException e) {
                closed = ex(e);
            }
            row("raf.after-close", closed);
        }
        // read-only file refuses write
        try (RandomAccessFile ro = new RandomAccessFile(f, "r")) {
            try { ro.write(1); row("raf.ro-write", "no-throw"); } catch (IOException e) { row("raf.ro-write", ex(e)); }
            byte[] b = new byte[10];
            row("raf.ro-read", ro.read(b));
        }

        // ---- identity / virtual natives through compiled sites ----
        Object o = new Object();
        row("id.stable", idStable(o));
        row("id.null", System.identityHashCode(null));
        row("hash.stable", hashStable(new Object()));
        row("hash.arr", hashStable(new int[3]));
        row("hash.poly", polyHash(new Object[] {new Object(), new int[2], new A(), "s", new Object[0], new StringBuilder()}));
        row("hash.null", nullReceiver(null));
        row("arraycopy.null", nullArg());
        row("arraycopy.oob", arrayStore());

        // ---- polymorphic site where the native receiver class changes under a warm site ----
        List<Object> mix = new ArrayList<>();
        for (int i = 0; i < 64; i++) mix.add(i % 3 == 0 ? (Object) new Object() : i % 3 == 1 ? (Object) new int[i] : (Object) new Shape[0]);
        int agree = 0;
        for (int r = 0; r < 500; r++) for (Object m : mix) if (m.hashCode() == System.identityHashCode(m)) agree++;
        row("poly.agree", agree);

        // ---- Boolean.valueOf identity under a compiled loop ----
        int idt = 0;
        for (int i = 0; i < HOT; i++) {
            Boolean b = Boolean.valueOf((i & 1) == 0);
            if ((i & 1) == 0 ? b == Boolean.TRUE : b == Boolean.FALSE) idt++;
        }
        row("bool.identity", idt);

        // ---- a native that throws from a compiled loop, caught, loop continues ----
        int caught = 0;
        for (int i = 0; i < HOT; i++) {
            try { Class.forName("no.such.Klass" + (i & 3)); } catch (ClassNotFoundException e) { caught++; }
        }
        row("throw.caught", caught);

        // ---- blocking-region natives (FileInputStream/FileOutputStream) from compiled loops ----
        File g = File.createTempFile("ncallg", ".bin");
        g.deleteOnExit();
        try (java.io.FileOutputStream fo = new java.io.FileOutputStream(g)) {
            for (int i = 0; i < 300; i++) fo.write(data, 0, 4096);
        }
        row("fos.length", g.length());
        long fsum = 0;
        int chunks = 0;
        try (java.io.FileInputStream fi = new java.io.FileInputStream(g)) {
            byte[] cb = new byte[1000];
            int n;
            while ((n = fi.read(cb, 0, 1000)) > 0) {
                chunks++;
                for (int i = 0; i < n; i += 97) fsum += cb[i];
            }
            row("fis.eof", fi.read());
            row("fis.avail", fi.available());
        }
        row("fis.chunks", chunks);
        row("fis.sum", fsum);
        java.io.FileInputStream fc = new java.io.FileInputStream(g);
        fc.close();
        String fclosed = "no-throw";
        try {
            byte[] cb = new byte[10];
            for (int i = 0; i < HOT; i++) fc.read(cb, 0, 10);
        } catch (IOException e) {
            fclosed = ex(e);
        }
        row("fis.after-close", fclosed);
        long skipped = 0;
        try (java.io.FileInputStream fi = new java.io.FileInputStream(g)) {
            skipped = fi.skip(5000);
            row("fis.skip", skipped + " next=" + fi.read());
        }

        // ---- positive control (informational): a 1 KB RAF read is leaf-served ----
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            raf.write(data, 0, 4096);
            byte[] buf = new byte[1024];
            for (int i = 0; i < 20000; i++) { raf.seek(0); raf.readFully(buf, 0, 1024); }
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                for (int i = 0; i < 50000; i++) { raf.seek(0); raf.readFully(buf, 0, 1024); }
                best = Math.min(best, (System.nanoTime() - t0) / 50000);
            }
            System.out.println("speed: raf1k-read " + best + " ns/op");
        }
    }
}
