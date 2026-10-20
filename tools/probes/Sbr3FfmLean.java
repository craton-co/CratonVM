import java.io.RandomAccessFile;
import java.lang.foreign.*;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * SBR3 lane ffmperf: the lean real-segment access (`segment_vh_lean`, the JIT funnel's `try_varhandle_segment_access`).
 * Every access shape is run COLD (interpreted) and after a warm-up large enough for the JIT to compile it, and every
 * failure path is exercised from compiled code too, so a lean decline that did not reach the unchanged road shows up as
 * a changed exception or value. Output is diffed against HotSpot (addresses normalised).
 */
public class Sbr3FfmLean {
    static long sink;
    static void row(String k, Object v) { System.out.println(k + " |" + v + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String k, T body) {
        try { row(k, body.run()); }
        catch (Throwable e) { row(k, "THREW " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    static String norm(Throwable e) { return e.getClass().getName() + ": " + String.valueOf(e.getMessage()).replaceAll("0x[0-9a-f]+|address: \\d+", "A"); }
    static void tn(String k, T body) {
        try { row(k, body.run()); }
        catch (Throwable e) { row(k, "THREW " + norm(e)); }
    }

    static final int WARM = 60000;

    public static void main(String[] args) throws Throwable {
        // ---- value shapes, cold then hot, native confined
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64, 8);
            for (int round = 0; round < 2; round++) {
                String ph = round == 0 ? "cold" : "hot";
                int n = round == 0 ? 3 : WARM;
                long acc = 0;
                for (int i = 0; i < n; i++) {
                    s.set(ValueLayout.JAVA_BYTE, 1, (byte) (i - 7));
                    s.set(ValueLayout.JAVA_SHORT, 2, (short) (i * 3 - 9));
                    s.set(ValueLayout.JAVA_CHAR, 4, (char) (i + 40000));
                    s.set(ValueLayout.JAVA_INT, 8, i * 31 - 77);
                    s.set(ValueLayout.JAVA_LONG, 16, (long) i * 1000003L - 5L);
                    s.set(ValueLayout.JAVA_FLOAT, 24, i * 1.5f - 2f);
                    s.set(ValueLayout.JAVA_DOUBLE, 32, i * 2.25 - 3.0);
                    s.set(ValueLayout.JAVA_BOOLEAN, 40, (i & 1) == 0);
                    s.set(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 44, 0x01020304 + i);
                    s.set(ValueLayout.JAVA_INT_UNALIGNED, 49, i ^ 0x5a5a);
                    acc += s.get(ValueLayout.JAVA_BYTE, 1) + s.get(ValueLayout.JAVA_SHORT, 2) + s.get(ValueLayout.JAVA_CHAR, 4)
                        + s.get(ValueLayout.JAVA_INT, 8) + s.get(ValueLayout.JAVA_LONG, 16) + (long) s.get(ValueLayout.JAVA_FLOAT, 24)
                        + (long) s.get(ValueLayout.JAVA_DOUBLE, 32) + (s.get(ValueLayout.JAVA_BOOLEAN, 40) ? 1 : 0)
                        + s.get(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 44) + s.get(ValueLayout.JAVA_INT_UNALIGNED, 49);
                }
                row("shapes." + ph + ".acc", acc);
                row("shapes." + ph + ".byte", s.get(ValueLayout.JAVA_BYTE, 1));
                row("shapes." + ph + ".int", s.get(ValueLayout.JAVA_INT, 8));
                row("shapes." + ph + ".be", Integer.toHexString(s.get(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 44)));
                row("shapes." + ph + ".rawbe", Integer.toHexString(s.get(ValueLayout.JAVA_INT, 44)));
                row("shapes." + ph + ".unal", s.get(ValueLayout.JAVA_INT_UNALIGNED, 49));
                row("shapes." + ph + ".bool", s.get(ValueLayout.JAVA_BOOLEAN, 40));
            }
            // ---- failure paths from COMPILED code (warm the access first, then fail it)
            for (int i = 0; i < WARM; i++) { s.set(ValueLayout.JAVA_INT, (i & 7) << 2, i); sink += s.get(ValueLayout.JAVA_INT, (i & 7) << 2); }
            tn("hot.oob.get", () -> s.get(ValueLayout.JAVA_INT, 62));
            tn("hot.oob.set", () -> { s.set(ValueLayout.JAVA_LONG, 60, 1L); return "no"; });
            tn("hot.neg", () -> s.get(ValueLayout.JAVA_INT, -4));
            tn("hot.misaligned.get", () -> s.get(ValueLayout.JAVA_INT, 2));
            tn("hot.misaligned.set", () -> { s.set(ValueLayout.JAVA_LONG, 4, 1L); return "no"; });
            MemorySegment ro = s.asReadOnly();
            for (int i = 0; i < WARM; i++) sink += ro.get(ValueLayout.JAVA_INT, (i & 7) << 2);
            tn("hot.ro.get", () -> ro.get(ValueLayout.JAVA_INT, 4));
            tn("hot.ro.set", () -> { ro.set(ValueLayout.JAVA_INT, 4, 1); return "no"; });
            MemorySegment slice = s.asSlice(8, 16);
            for (int i = 0; i < WARM; i++) sink += slice.get(ValueLayout.JAVA_INT, (i & 3) << 2);
            tn("hot.slice.ok", () -> slice.get(ValueLayout.JAVA_INT, 12));
            tn("hot.slice.oob", () -> slice.get(ValueLayout.JAVA_INT, 16));
            tn("hot.slice.align", () -> slice.get(ValueLayout.JAVA_LONG, 4));
        }
        // ---- closed session after a hot loop
        {
            Arena a = Arena.ofConfined();
            MemorySegment s = a.allocate(32, 8);
            for (int i = 0; i < WARM; i++) { s.set(ValueLayout.JAVA_LONG, (i & 3) << 3, i); sink += s.get(ValueLayout.JAVA_LONG, (i & 3) << 3); }
            row("closed.before", s.get(ValueLayout.JAVA_LONG, 8));
            a.close();
            tn("closed.get", () -> s.get(ValueLayout.JAVA_LONG, 8));
            tn("closed.set", () -> { s.set(ValueLayout.JAVA_LONG, 8, 1L); return "no"; });
        }
        // ---- confined arena touched by another thread (hot in the owner first)
        {
            Arena a = Arena.ofConfined();
            MemorySegment s = a.allocate(32, 8);
            for (int i = 0; i < WARM; i++) { s.set(ValueLayout.JAVA_INT, (i & 3) << 2, i); sink += s.get(ValueLayout.JAVA_INT, (i & 3) << 2); }
            Object[] out = new Object[2];
            Thread th = new Thread(() -> {
                try { out[0] = s.get(ValueLayout.JAVA_INT, 4); } catch (Throwable e) { out[0] = norm(e); }
                try { s.set(ValueLayout.JAVA_INT, 4, 9); out[1] = "no"; } catch (Throwable e) { out[1] = norm(e); }
            });
            th.start(); th.join();
            row("thread.get", out[0]); row("thread.set", out[1]);
            a.close();
        }
        // ---- shared arena, many threads, hot, then close while a reader is gone
        {
            Arena a = Arena.ofShared();
            MemorySegment s = a.allocate(4096, 8);
            Thread[] ts = new Thread[4];
            long[] sums = new long[4];
            for (int k = 0; k < ts.length; k++) {
                final int id = k;
                ts[k] = new Thread(() -> {
                    long acc = 0;
                    for (int i = 0; i < 20000; i++) {
                        int off = ((i * 7) & 255) << 2;
                        s.set(ValueLayout.JAVA_INT, off, id + i);
                        acc += s.get(ValueLayout.JAVA_INT, off) >= 0 ? 1 : 0;
                    }
                    sums[id] = acc;
                });
                ts[k].start();
            }
            for (Thread th : ts) th.join();
            row("shared.sums", java.util.Arrays.toString(sums));
            a.close();
            tn("shared.closed", () -> s.get(ValueLayout.JAVA_INT, 0));
        }
        // ---- global and auto arenas
        {
            MemorySegment g = Arena.global().allocate(16, 8);
            MemorySegment au = Arena.ofAuto().allocate(16, 8);
            for (int i = 0; i < WARM; i++) { g.set(ValueLayout.JAVA_INT, 4, i); au.set(ValueLayout.JAVA_LONG, 8, i); sink += g.get(ValueLayout.JAVA_INT, 4) + au.get(ValueLayout.JAVA_LONG, 8); }
            row("global.get", g.get(ValueLayout.JAVA_INT, 4));
            row("auto.get", au.get(ValueLayout.JAVA_LONG, 8));
        }
        // ---- heap segments and raw-address windows (the lean path declines / dereferences)
        {
            int[] arr = new int[16];
            MemorySegment h = MemorySegment.ofArray(arr);
            for (int i = 0; i < WARM; i++) { h.set(ValueLayout.JAVA_INT, (i & 15) << 2, i); sink += h.get(ValueLayout.JAVA_INT, (i & 15) << 2); }
            row("heap.arr", java.util.Arrays.toString(arr));
            tn("heap.oob", () -> h.get(ValueLayout.JAVA_INT, 64));
            tn("heap.align", () -> h.get(ValueLayout.JAVA_LONG, 4));
            try (Arena a = Arena.ofConfined()) {
                MemorySegment s = a.allocate(32, 8);
                MemorySegment w = MemorySegment.ofAddress(s.address()).reinterpret(32);
                for (int i = 0; i < WARM; i++) { w.set(ValueLayout.JAVA_INT, (i & 7) << 2, i); sink += w.get(ValueLayout.JAVA_INT, (i & 7) << 2); }
                row("window.get", w.get(ValueLayout.JAVA_INT, 4));
                row("window.shared", s.get(ValueLayout.JAVA_INT, 4));
            }
        }
        // ---- mapped file
        {
            Path p = Files.createTempFile("sbr3lean", ".bin");
            try (FileChannel ch = FileChannel.open(p, java.nio.file.StandardOpenOption.READ, java.nio.file.StandardOpenOption.WRITE);
                 Arena a = Arena.ofConfined()) {
                MemorySegment m = ch.map(FileChannel.MapMode.READ_WRITE, 0, 4096, a);
                for (int i = 0; i < WARM; i++) { m.set(ValueLayout.JAVA_INT, (i & 255) << 2, i); sink += m.get(ValueLayout.JAVA_INT, (i & 255) << 2); }
                row("mapped.get", m.get(ValueLayout.JAVA_INT, 4));
                tn("mapped.oob", () -> m.get(ValueLayout.JAVA_INT, 4096));
                m.force();
            } finally { Files.deleteIfExists(p); }
        }
        // ---- explicit VarHandles (the same road: plain, path, sequence element, struct member)
        try (Arena a = Arena.ofConfined()) {
            VarHandle vh = ValueLayout.JAVA_INT.varHandle();
            MemorySegment s = a.allocate(64, 8);
            for (int i = 0; i < WARM; i++) { vh.set(s, (long) ((i & 15) << 2), i); sink += (int) vh.get(s, (long) ((i & 15) << 2)); }
            row("vh.get", (int) vh.get(s, 4L));
            tn("vh.oob", () -> (int) vh.get(s, 64L));
            tn("vh.wrongtype", () -> { vh.set(s, 0L, "s"); return "no"; });
            StructLayout st = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y"), ValueLayout.JAVA_LONG.withName("z"));
            VarHandle vy = st.varHandle(MemoryLayout.PathElement.groupElement("y"));
            for (int i = 0; i < WARM; i++) { vy.set(s, 0L, i); sink += (int) vy.get(s, 0L); }
            row("struct.y", (int) vy.get(s, 0L));
            VarHandle ve = ValueLayout.JAVA_INT.arrayElementVarHandle();
            for (int i = 0; i < 2000; i++) { ve.set(s, 0L, (long) (i & 15), i); sink += (int) ve.get(s, 0L, (long) (i & 15)); }
            row("arrayElement", (int) ve.get(s, 0L, 3L));
            tn("arrayElement.oob", () -> (int) ve.get(s, 0L, 16L));
        }
        // ---- the sink keeps the loops live
        row("sink", sink != 42);
    }
}
