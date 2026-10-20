import java.lang.foreign.*;
import java.lang.management.*;
import java.lang.invoke.*;
import java.nio.*;
import java.util.*;

/** SBR5 lane ffmlink: Buffer$2.reserveMemory accounting, ScopedMemoryAccess char unaligned, SymbolLookup.
 *  One deterministic line per row, diffed against HotSpot 25 (no addresses). */
public class Sbr5FfmlinkAcct {
    interface F { Object get() throws Throwable; }
    static void p(String tag, F f) {
        String v;
        try { v = String.valueOf(f.get()); }
        catch (Throwable e) { v = "THREW " + e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(tag + " |" + v + "|");
    }
    static BufferPoolMXBean direct() {
        for (BufferPoolMXBean b : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class))
            if (b.getName().equals("direct")) return b;
        return null;
    }
    public static void main(String[] a) throws Throwable {
        BufferPoolMXBean d = direct();
        long c0 = d.getCount(), u0 = d.getMemoryUsed(), t0 = d.getTotalCapacity();
        // ---- accounting
        Arena ar = Arena.ofConfined();
        MemorySegment s1 = ar.allocate(1000);
        MemorySegment s2 = ar.allocate(5000, 16);
        p("acct.count.up", () -> d.getCount() - c0);
        p("acct.used.up", () -> d.getMemoryUsed() - u0);
        p("acct.cap.up", () -> d.getTotalCapacity() - t0);
        ar.close();
        p("acct.count.down", () -> d.getCount() - c0);
        p("acct.used.down", () -> d.getMemoryUsed() - u0);
        p("acct.cap.down", () -> d.getTotalCapacity() - t0);
        // shared arena + direct buffer alongside
        try (Arena sh = Arena.ofShared()) {
            sh.allocate(4096);
            ByteBuffer bb = ByteBuffer.allocateDirect(2048);
            p("acct.mixed.count", () -> d.getCount() - c0);
            p("acct.mixed.used", () -> d.getMemoryUsed() - u0);
            p("acct.bb.cap", () -> bb.capacity());
        }
        p("acct.mixed.after.count", () -> d.getCount() - c0 <= 1);
        // zero-size and failures keep the books straight
        p("acct.zero", () -> { try (Arena z = Arena.ofConfined()) { z.allocate(0); return d.getCount() - c0; } });
        p("acct.neg", () -> { try (Arena z = Arena.ofConfined()) { z.allocate(-1); return "no"; } });
        p("acct.after.fail.count", () -> d.getCount() - c0);
        p("acct.huge", () -> { try (Arena z = Arena.ofConfined()) { z.allocate(Long.MAX_VALUE / 2); return "no"; } });
        p("acct.after.huge.count", () -> d.getCount() - c0);
        p("acct.after.huge.used", () -> d.getMemoryUsed() - u0);
        p("acct.auto", () -> { Arena au = Arena.ofAuto(); au.allocate(100); return d.getCount() - c0 >= 1; });
        // many alloc/free cycles do not drift
        for (int i = 0; i < 200; i++) try (Arena z = Arena.ofConfined()) { z.allocate(64 + i); }
        p("acct.cycles.used", () -> d.getMemoryUsed() - u0 < 4096 + 100_000);
        // ---- char unaligned (Heap/Direct ByteBuffer + segments)
        for (ByteOrder bo : new ByteOrder[] { ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN }) {
            String t = bo == ByteOrder.BIG_ENDIAN ? "be" : "le";
            for (boolean dir : new boolean[] { false, true }) {
                String k = "char." + t + (dir ? ".direct" : ".heap");
                ByteBuffer b = (dir ? ByteBuffer.allocateDirect(16) : ByteBuffer.allocate(16)).order(bo);
                p(k + ".put", () -> { b.putChar(1, 'ሴ'); b.putChar(3, '￾'); b.putChar(5, (char) 0x80FF); return b.get(1) + "," + b.get(2) + "," + b.get(3) + "," + b.get(4) + "," + b.get(5) + "," + b.get(6); });
                p(k + ".get", () -> (int) b.getChar(1) + "," + (int) b.getChar(3) + "," + (int) b.getChar(5));
                p(k + ".rel", () -> { b.clear(); b.putChar('A').putChar('퟿'); b.flip(); return (int) b.getChar() + "," + (int) b.getChar(); });
                p(k + ".oob", () -> b.getChar(15));
                p(k + ".oob.put", () -> { b.putChar(15, 'x'); return "no"; });
                p(k + ".as", () -> { CharBuffer cb = b.asCharBuffer(); cb.put(0, 'ÿ'); cb.put(1, '老'); return (int) cb.get(0) + "," + (int) cb.get(1) + "," + b.get(0) + "," + b.get(1); });
            }
        }
        try (Arena z = Arena.ofConfined()) {
            MemorySegment s = z.allocate(16);
            ValueLayout.OfChar cu = ValueLayout.JAVA_CHAR_UNALIGNED;
            ValueLayout.OfChar cb = ValueLayout.JAVA_CHAR_UNALIGNED.withOrder(ByteOrder.BIG_ENDIAN);
            p("char.seg.put", () -> { s.set(cu, 1, 'ሴ'); s.set(cb, 3, 'ሴ'); return s.get(ValueLayout.JAVA_BYTE, 1) + "," + s.get(ValueLayout.JAVA_BYTE, 2) + "," + s.get(ValueLayout.JAVA_BYTE, 3) + "," + s.get(ValueLayout.JAVA_BYTE, 4); });
            p("char.seg.get", () -> (int) s.get(cu, 1) + "," + (int) s.get(cb, 3) + "," + (int) s.get(cu, 3));
            p("char.seg.oob", () -> s.get(cu, 15));
            p("char.seg.aligned.bad", () -> s.get(ValueLayout.JAVA_CHAR, 1));
            p("char.seg.idx", () -> { s.setAtIndex(ValueLayout.JAVA_CHAR, 2, 'ꯍ'); return (int) s.getAtIndex(ValueLayout.JAVA_CHAR, 2); });
            MemorySegment heap = MemorySegment.ofArray(new byte[16]);
            p("char.heap.seg", () -> { heap.set(cu, 3, 'Ă'); return (int) heap.get(cu, 3) + "," + heap.get(ValueLayout.JAVA_BYTE, 3) + "," + heap.get(ValueLayout.JAVA_BYTE, 4); });
        }
        try (Arena z = Arena.ofConfined()) {
            MemorySegment s = z.allocate(16);
            Arena other = z;
            p("char.closed", () -> { Arena c = Arena.ofConfined(); MemorySegment q = c.allocate(8); c.close(); return q.get(ValueLayout.JAVA_CHAR_UNALIGNED, 1); });
            p("char.closed.put", () -> { Arena c = Arena.ofConfined(); MemorySegment q = c.allocate(8); c.close(); q.set(ValueLayout.JAVA_CHAR_UNALIGNED, 1, 'x'); return "no"; });
            Throwable[] th = new Throwable[1];
            Thread t = new Thread(() -> { try { s.get(ValueLayout.JAVA_CHAR_UNALIGNED, 1); } catch (Throwable e) { th[0] = e; } });
            t.start(); t.join();
            System.out.println("char.thread |" + th[0] + "|");
        }
        // ---- SymbolLookup
        p("lookup.loader.missing", () -> SymbolLookup.loaderLookup().find("no_such_symbol_zz").isPresent());
        p("lookup.loader.or", () -> SymbolLookup.loaderLookup().or(SymbolLookup.loaderLookup()).find("zzz").isPresent());
        p("lookup.lib.missing", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup("no_such_lib_zz.so", z); } });
        p("lookup.lib.nul", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup("a\0b", z); } });
        p("lookup.lib.null", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup((String) null, z); } });
        p("lookup.lib.nullarena", () -> SymbolLookup.libraryLookup("libc.so.6", null));
        p("lookup.lib.libc", () -> { try (Arena z = Arena.ofConfined()) { SymbolLookup l = SymbolLookup.libraryLookup("libc.so.6", z); return l.find("strlen").isPresent() + " " + l.find("no_such_symbol_zz").isPresent(); } });
        p("lookup.lib.closed", () -> { Arena z = Arena.ofConfined(); SymbolLookup l = SymbolLookup.libraryLookup("libc.so.6", z); MemorySegment m = l.find("strlen").get(); z.close(); return m.scope().isAlive() + " " + l.find("strlen").isPresent(); });
        p("lookup.lib.find.null", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup("libc.so.6", z).find(null); } });
        p("lookup.lib.path", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup(java.nio.file.Path.of("/no/such/lib.so"), z); } });
        p("lookup.lib.or", () -> { try (Arena z = Arena.ofConfined()) { return SymbolLookup.libraryLookup("libc.so.6", z).or(SymbolLookup.loaderLookup()).find("strlen").isPresent(); } });
        p("lookup.lib.call", () -> {
            try (Arena z = Arena.ofConfined()) {
                SymbolLookup l = SymbolLookup.libraryLookup("libc.so.6", z);
                MethodHandle h = Linker.nativeLinker().downcallHandle(l.find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
                return (long) h.invokeExact(z.allocateFrom("hello world"));
            }
        });
        p("lookup.default.call", () -> {
            try (Arena z = Arena.ofConfined()) {
                MethodHandle h = Linker.nativeLinker().downcallHandle(Linker.nativeLinker().defaultLookup().find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
                return (long) h.invokeExact(z.allocateFrom("abc"));
            }
        });
        p("lookup.loader.system", () -> { System.loadLibrary("zip"); return SymbolLookup.loaderLookup().find("ZIP_Open").isPresent() + " " + SymbolLookup.loaderLookup().find("Java_java_util_zip_Inflater_init").isPresent(); });
    }
}
