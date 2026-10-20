import java.lang.foreign.*;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * SBR4 lane segacc: the compiled `MemorySegment.get/set/getAtIndex/setAtIndex` door over REAL segments (the JDK's own
 * `NativeMemorySegmentImpl`). Every shape is run cold, then after a warm-up long enough for the single-pass JIT to compile
 * the loop, and every failure path is raised from COMPILED code after the access has been served there, so a decline that
 * did not reach the JDK's bytecode shows as a changed value or exception. Output is diffed against HotSpot (addresses
 * normalised). Last line: the JIT engagement line is on stderr (`ffm element fast path: ... hits=`).
 */
public class Sbr4SegaccIntr {
    static long sink;
    static void row(String k, Object v) { System.out.println(k + " |" + v + "|"); }
    interface T { Object run() throws Throwable; }
    static String norm(Throwable e) { return e.getClass().getName() + ": " + String.valueOf(e.getMessage()).replaceAll("0x[0-9a-f]+|address: \\d+|Thread\\[[^]]*\\]", "A"); }
    static void tn(String k, T body) {
        try { row(k, body.run()); }
        catch (Throwable e) { row(k, "THREW " + norm(e)); }
    }
    static final int WARM = 60000;

    static long kinds(MemorySegment s, int i) {
        s.set(ValueLayout.JAVA_BYTE, 1, (byte) (i - 7));
        s.set(ValueLayout.JAVA_SHORT, 2, (short) (i * 3 - 9));
        s.set(ValueLayout.JAVA_CHAR, 4, (char) (i + 40000));
        s.set(ValueLayout.JAVA_INT, 8, i * 31 - 77);
        s.set(ValueLayout.JAVA_LONG, 16, (long) i * 1000003L - 5L);
        s.set(ValueLayout.JAVA_FLOAT, 24, i * 1.5f - 2f);
        s.set(ValueLayout.JAVA_DOUBLE, 32, i * 2.25 - 3.0);
        return s.get(ValueLayout.JAVA_BYTE, 1) + s.get(ValueLayout.JAVA_SHORT, 2) + s.get(ValueLayout.JAVA_CHAR, 4)
            + s.get(ValueLayout.JAVA_INT, 8) + s.get(ValueLayout.JAVA_LONG, 16) + (long) s.get(ValueLayout.JAVA_FLOAT, 24)
            + (long) s.get(ValueLayout.JAVA_DOUBLE, 32);
    }
    static long atIndex(MemorySegment s, int i) {
        s.setAtIndex(ValueLayout.JAVA_BYTE, 40 + (i & 7), (byte) i);
        s.setAtIndex(ValueLayout.JAVA_SHORT, 24 + (i & 3), (short) (i * 5));
        s.setAtIndex(ValueLayout.JAVA_CHAR, 28 + (i & 3), (char) (i * 7));
        s.setAtIndex(ValueLayout.JAVA_INT, 12 + (i & 3), i * 11);
        s.setAtIndex(ValueLayout.JAVA_LONG, 4 + (i & 1), (long) i * 13);
        s.setAtIndex(ValueLayout.JAVA_FLOAT, 6 + (i & 1), i * 0.5f);
        s.setAtIndex(ValueLayout.JAVA_DOUBLE, 5, i * 0.25);
        return s.getAtIndex(ValueLayout.JAVA_BYTE, 40 + (i & 7)) + s.getAtIndex(ValueLayout.JAVA_SHORT, 24 + (i & 3))
            + s.getAtIndex(ValueLayout.JAVA_CHAR, 28 + (i & 3)) + s.getAtIndex(ValueLayout.JAVA_INT, 12 + (i & 3))
            + s.getAtIndex(ValueLayout.JAVA_LONG, 4 + (i & 1)) + (long) s.getAtIndex(ValueLayout.JAVA_FLOAT, 6 + (i & 1))
            + (long) s.getAtIndex(ValueLayout.JAVA_DOUBLE, 5);
    }
    static int geti(MemorySegment s, long off) { return s.get(ValueLayout.JAVA_INT, off); }
    static void seti(MemorySegment s, long off, int v) { s.set(ValueLayout.JAVA_INT, off, v); }
    static int getiAt(MemorySegment s, long idx) { return s.getAtIndex(ValueLayout.JAVA_INT, idx); }
    static long getl(MemorySegment s, ValueLayout.OfLong l, long off) { return s.get(l, off); }
    static int getiL(MemorySegment s, ValueLayout.OfInt l, long off) { return s.get(l, off); }
    static int getiAtL(MemorySegment s, ValueLayout.OfInt l, long idx) { return s.getAtIndex(l, idx); }

    public static void main(String[] args) throws Throwable {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(128, 8);
            for (int round = 0; round < 2; round++) {
                String ph = round == 0 ? "cold" : "hot";
                int n = round == 0 ? 3 : WARM;
                long acc = 0, acc2 = 0;
                for (int i = 0; i < n; i++) { acc += kinds(s, i); acc2 += atIndex(s, i); }
                row("kinds." + ph, acc);
                row("atIndex." + ph, acc2);
                row("kinds." + ph + ".int", s.get(ValueLayout.JAVA_INT, 8));
                row("atIndex." + ph + ".long", s.getAtIndex(ValueLayout.JAVA_LONG, 4));
            }
            // alignment / order / unaligned layouts, hot
            ValueLayout.OfInt be = ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN);
            ValueLayout.OfInt un = ValueLayout.JAVA_INT_UNALIGNED;
            ValueLayout.OfInt a8 = ValueLayout.JAVA_INT.withByteAlignment(8);
            long acc = 0;
            for (int i = 0; i < WARM; i++) {
                s.set(be, 60, 0x01020304 + i); s.set(un, 65, i ^ 0x5a5a);
                acc += getiL(s, be, 60) + getiL(s, un, 65) + getiL(s, ValueLayout.JAVA_INT, 60) + getiL(s, a8, 64);
                acc += getiAtL(s, be, 15) + getiAtL(s, un, 16);
            }
            row("layouts.acc", acc);
            tn("a8.get.misaligned", () -> getiL(s, a8, 68));
            tn("a8.getAtIndex", () -> getiAtL(s, a8, 1));
            tn("un.getAtIndex", () -> getiAtL(s, un, 1));
            // failures raised from compiled code (the access has been served there)
            for (int i = 0; i < WARM; i++) { seti(s, (i & 7) << 2, i); sink += geti(s, (i & 7) << 2) + getiAt(s, i & 7); }
            tn("hot.oob.get", () -> geti(s, 126));
            tn("hot.oob.set", () -> { seti(s, 126, 1); return "no"; });
            tn("hot.neg", () -> geti(s, -4));
            tn("hot.oob.idx", () -> getiAt(s, 32));
            tn("hot.neg.idx", () -> getiAt(s, -1));
            tn("hot.huge.idx", () -> getiAt(s, Long.MAX_VALUE / 2));
            tn("hot.misaligned.get", () -> geti(s, 2));
            tn("hot.misaligned.long", () -> getl(s, ValueLayout.JAVA_LONG, 4));
            MemorySegment ro = s.asReadOnly();
            for (int i = 0; i < WARM; i++) sink += geti(ro, (i & 7) << 2);
            tn("hot.ro.get", () -> geti(ro, 4));
            tn("hot.ro.set", () -> { seti(ro, 4, 1); return "no"; });
            MemorySegment slice = s.asSlice(8, 16);
            for (int i = 0; i < WARM; i++) sink += geti(slice, (i & 3) << 2);
            tn("hot.slice.ok", () -> geti(slice, 12));
            tn("hot.slice.oob", () -> geti(slice, 16));
            MemorySegment odd = s.asSlice(4, 24);
            for (int i = 0; i < WARM; i++) sink += geti(odd, (i & 3) << 2);
            tn("hot.slice.align", () -> getl(odd, ValueLayout.JAVA_LONG, 0));
            // a thread that does not own the confined arena
            Throwable[] t2 = new Throwable[1];
            Thread th = new Thread(() -> { try { geti(s, 4); } catch (Throwable e) { t2[0] = e; } });
            th.start(); th.join();
            row("hot.foreign.thread", t2[0] == null ? "no exception" : norm(t2[0]));
        }
        // closed confined session after a hot loop, then every access shape
        Arena c = Arena.ofConfined();
        MemorySegment cs = c.allocate(64, 8);
        for (int i = 0; i < WARM; i++) { seti(cs, (i & 7) << 2, i); sink += geti(cs, (i & 7) << 2); }
        c.close();
        tn("closed.get", () -> geti(cs, 0));
        tn("closed.set", () -> { seti(cs, 0, 1); return "no"; });
        tn("closed.getAtIndex", () -> getiAt(cs, 0));
        // shared: written by one thread, read by another, then closed under a reader loop
        Arena sh = Arena.ofShared();
        MemorySegment ss = sh.allocate(64, 8);
        for (int i = 0; i < WARM; i++) { seti(ss, (i & 7) << 2, i); sink += geti(ss, (i & 7) << 2); }
        long[] seen = new long[1];
        Thread rd = new Thread(() -> { long x = 0; for (int i = 0; i < WARM; i++) x += geti(ss, (i & 7) << 2); seen[0] = x; });
        rd.start(); rd.join();
        row("shared.other.thread", seen[0]);
        sh.close();
        tn("shared.closed.get", () -> geti(ss, 0));
        // global / auto
        MemorySegment g = Arena.global().allocate(64, 8);
        MemorySegment au = Arena.ofAuto().allocate(64, 8);
        long gacc = 0;
        for (int i = 0; i < WARM; i++) { seti(g, (i & 7) << 2, i); seti(au, (i & 7) << 2, i * 2); gacc += geti(g, (i & 7) << 2) + geti(au, (i & 7) << 2); }
        row("global.auto.acc", gacc);
        // an ofAddress window reinterpreted over a live block
        try (Arena a = Arena.ofConfined()) {
            MemorySegment base = a.allocate(64, 8);
            MemorySegment win = MemorySegment.ofAddress(base.address()).reinterpret(64);
            long wacc = 0;
            for (int i = 0; i < WARM; i++) { seti(win, (i & 7) << 2, i); wacc += geti(base, (i & 7) << 2) + geti(win, (i & 7) << 2); }
            row("window.acc", wacc);
            tn("window.oob", () -> geti(win, 64));
        }
        // a mapped file
        Path f = Files.createTempFile("sbr4segacc", ".bin");
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE); Arena a = Arena.ofConfined()) {
            MemorySegment m = ch.map(FileChannel.MapMode.READ_WRITE, 0, 4096, a);
            long macc = 0;
            for (int i = 0; i < WARM; i++) { seti(m, (i & 1023) << 2, i); macc += geti(m, ((i * 7) & 1023) << 2); }
            row("mapped.acc", macc);
            tn("mapped.oob", () -> geti(m, 4096));
        } finally { Files.deleteIfExists(f); }
        // heap segments stay on the unchanged road
        int[] arr = new int[64];
        MemorySegment h = MemorySegment.ofArray(arr);
        long hacc = 0;
        for (int i = 0; i < WARM; i++) { seti(h, (i & 63) << 2, i); hacc += geti(h, (i & 63) << 2) + getiAt(h, i & 63); }
        row("heap.acc", hacc);
        tn("heap.misaligned", () -> getl(h, ValueLayout.JAVA_LONG, 4));
        // a segment that changes receiver class at one site (native, then heap, then native)
        MemorySegment[] mix = new MemorySegment[3];
        Arena ma = Arena.ofConfined();
        mix[0] = ma.allocate(64, 8); mix[1] = h; mix[2] = ma.allocate(64, 8);
        long xacc = 0;
        for (int i = 0; i < WARM; i++) { MemorySegment m = mix[i % 3]; seti(m, (i & 7) << 2, i); xacc += geti(m, (i & 7) << 2); }
        row("mixed.acc", xacc);
        ma.close();
        System.out.println("sink " + (sink != 42));
    }
}
