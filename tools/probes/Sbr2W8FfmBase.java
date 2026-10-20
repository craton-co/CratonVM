import java.lang.foreign.*;
import java.lang.invoke.*;
import java.nio.ByteOrder;
import java.util.*;
import java.util.stream.*;

/** SBR2 wave 8, lane FFM: the java.lang.foreign surface, one deterministic line per row (no addresses).
 *  Diffed against HotSpot 25; rows ask behaviour, exception type+message, never the carrier class name. */
public class Sbr2W8FfmBase {
    interface F { Object get() throws Throwable; }
    static String[] skip = new String[0];
    static void p(String tag, F f) {
        for (String k : skip) if (tag.equals(k)) { System.out.println(tag + " SKIPPED"); return; }
        String v;
        try { v = String.valueOf(f.get()); }
        catch (Throwable e) { v = "THREW " + e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(tag + " |" + v + "|");
    }
    static void sect(String name, Runnable r) {
        try { r.run(); } catch (Throwable e) { System.out.println("SECTION-ABORTED " + name + " " + e); }
    }

    public static void main(String[] a) throws Throwable {
        if (a.length > 0) skip = a[0].split(",");
        sect("arena", Sbr2W8FfmBase::arena);
        sect("segment", Sbr2W8FfmBase::segment);
        sect("slice", Sbr2W8FfmBase::slice);
        sect("copy", Sbr2W8FfmBase::copy);
        sect("closed", Sbr2W8FfmBase::closed);
        sect("thread", Sbr2W8FfmBase::thread);
        sect("layout", Sbr2W8FfmBase::layout);
        sect("vh", Sbr2W8FfmBase::vh);
        sect("fd", Sbr2W8FfmBase::fd);
        sect("misc", Sbr2W8FfmBase::misc);
    }

    static void arena() {
        p("auto.alive", () -> Arena.ofAuto().scope().isAlive());
        p("global.alive", () -> Arena.global().scope().isAlive());
        p("global.same", () -> Arena.global() == Arena.global());
        p("global.close", () -> { Arena.global().close(); return "no"; });
        p("auto.close", () -> { Arena.ofAuto().close(); return "no"; });
        p("confined.alive", () -> { try (Arena x = Arena.ofConfined()) { return x.scope().isAlive(); } });
        p("shared.alive", () -> { try (Arena x = Arena.ofShared()) { return x.scope().isAlive(); } });
        p("closed.alive", () -> { Arena x = Arena.ofConfined(); x.close(); return x.scope().isAlive(); });
        p("double.close", () -> { Arena x = Arena.ofConfined(); x.close(); x.close(); return "no"; });
        p("shared.double.close", () -> { Arena x = Arena.ofShared(); x.close(); x.close(); return "no"; });
        p("alloc.size", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(24).byteSize(); } });
        p("alloc.align", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(24, 16).address() % 16; } });
        p("alloc.badalign", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(24, 3).byteSize(); } });
        p("alloc.neg", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(-1).byteSize(); } });
        p("alloc.zero", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(0).byteSize(); } });
        p("alloc.zeroed", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(64); long t = 0; for (int i = 0; i < 64; i++) t += s.get(ValueLayout.JAVA_BYTE, i); return t; } });
        p("allocFrom.str", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocateFrom("hello"); return s.byteSize() + " " + s.getString(0); } });
        p("allocFrom.int", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_INT, 42).get(ValueLayout.JAVA_INT, 0); } });
        p("allocFrom.ints", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocateFrom(ValueLayout.JAVA_INT, 1, 2, 3); return s.byteSize() + " " + s.getAtIndex(ValueLayout.JAVA_INT, 2); } });
        p("allocateArray", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(ValueLayout.JAVA_LONG, 3).byteSize(); } });
        p("allocate.layout", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(ValueLayout.JAVA_LONG).byteSize(); } });
        p("scope.eq", () -> { try (Arena x = Arena.ofConfined()) { return x.scope() == x.allocate(8).scope(); } });
        p("scope.equals.other", () -> { try (Arena x = Arena.ofConfined(); Arena y = Arena.ofConfined()) { return x.scope().equals(y.scope()); } });
        p("seg.scope.global", () -> MemorySegment.NULL.scope().isAlive());
        p("ofAddress.scope", () -> MemorySegment.ofAddress(1234).scope().isAlive());
        p("ofAddress.size", () -> MemorySegment.ofAddress(1234).byteSize());
        p("ofAddress.addr", () -> MemorySegment.ofAddress(1234).address());
        p("ofAddress.get", () -> MemorySegment.ofAddress(1234).get(ValueLayout.JAVA_BYTE, 0));
        p("reinterpret", () -> { try (Arena x = Arena.ofConfined()) { return MemorySegment.ofAddress(x.allocate(8).address()).reinterpret(8).byteSize(); } });
        p("NULL", () -> MemorySegment.NULL.address() + " " + MemorySegment.NULL.byteSize() + " " + MemorySegment.NULL.isNative());
        p("arena.alive.after", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.scope().isAlive(); });
    }

    static void segment() {
        p("heap.byte", () -> { byte[] b = new byte[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_BYTE, 3, (byte) 7); return b[3] + " " + s.byteSize() + " " + s.isNative(); });
        p("heap.char", () -> { char[] b = new char[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_CHAR, 2, 'x'); return (int) b[1] + " " + s.byteSize(); });
        p("heap.short", () -> { short[] b = new short[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_SHORT, 2, (short) 9); return b[1] + " " + s.byteSize(); });
        p("heap.int", () -> { int[] b = new int[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_INT, 4, 99); return b[1] + " " + s.byteSize(); });
        p("heap.long", () -> { long[] b = new long[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_LONG, 8, 99L); return b[1] + " " + s.byteSize(); });
        p("heap.float", () -> { float[] b = new float[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_FLOAT, 4, 1.5f); return b[1] + " " + s.byteSize(); });
        p("heap.double", () -> { double[] b = new double[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_DOUBLE, 8, 2.5); return b[1] + " " + s.byteSize(); });
        p("heap.misaligned", () -> { int[] b = new int[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_INT, 2, 1); return "ok"; });
        p("heap.misaligned.unaligned", () -> { int[] b = new int[8]; MemorySegment s = MemorySegment.ofArray(b); s.set(ValueLayout.JAVA_INT_UNALIGNED, 2, 1); return b[0]; });
        p("heap.array", () -> { int[] b = new int[4]; MemorySegment s = MemorySegment.ofArray(b); return s.heapBase().isPresent() + " " + (s.heapBase().get() == b); });
        p("heap.addr", () -> MemorySegment.ofArray(new byte[4]).address());
        p("heap.oob", () -> MemorySegment.ofArray(new byte[4]).get(ValueLayout.JAVA_INT, 1));
        p("heap.oob2", () -> MemorySegment.ofArray(new byte[4]).get(ValueLayout.JAVA_BYTE, 4));
        p("heap.neg", () -> MemorySegment.ofArray(new byte[4]).get(ValueLayout.JAVA_BYTE, -1));
        p("heap.null", () -> MemorySegment.ofArray((byte[]) null));
        p("native.getset", () -> { try (Arena x = Arena.ofConfined()) {
            MemorySegment s = x.allocate(64);
            s.set(ValueLayout.JAVA_BYTE, 0, (byte) -3); s.set(ValueLayout.JAVA_SHORT, 2, (short) -5); s.set(ValueLayout.JAVA_CHAR, 4, 'q');
            s.set(ValueLayout.JAVA_INT, 8, -7); s.set(ValueLayout.JAVA_LONG, 16, -9L); s.set(ValueLayout.JAVA_FLOAT, 24, 1.25f);
            s.set(ValueLayout.JAVA_DOUBLE, 32, 3.5); s.set(ValueLayout.JAVA_BOOLEAN, 40, true); s.set(ValueLayout.ADDRESS, 48, MemorySegment.ofAddress(77));
            return s.get(ValueLayout.JAVA_BYTE, 0) + " " + s.get(ValueLayout.JAVA_SHORT, 2) + " " + s.get(ValueLayout.JAVA_CHAR, 4) + " " + s.get(ValueLayout.JAVA_INT, 8) + " " + s.get(ValueLayout.JAVA_LONG, 16) + " " + s.get(ValueLayout.JAVA_FLOAT, 24) + " " + s.get(ValueLayout.JAVA_DOUBLE, 32) + " " + s.get(ValueLayout.JAVA_BOOLEAN, 40) + " " + s.get(ValueLayout.ADDRESS, 48).address();
        } });
        p("native.bigendian", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0, 0x01020304); return s.get(ValueLayout.JAVA_BYTE, 0) + " " + s.get(ValueLayout.JAVA_BYTE, 3); } });
        p("native.oob", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).get(ValueLayout.JAVA_LONG, 1); } });
        p("native.oob.end", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).get(ValueLayout.JAVA_INT, 8); } });
        p("native.oob.neg", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).get(ValueLayout.JAVA_INT, -4); } });
        p("native.oob.set", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).set(ValueLayout.JAVA_LONG, 4, 1L); return "no"; } });
        p("native.misaligned", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(16, 8).get(ValueLayout.JAVA_INT, 1); } });
        p("native.getAtIndex", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); s.setAtIndex(ValueLayout.JAVA_INT, 3, 5); return s.getAtIndex(ValueLayout.JAVA_INT, 3) + " " + s.get(ValueLayout.JAVA_INT, 12); } });
        p("native.getAtIndex.oob", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(16).getAtIndex(ValueLayout.JAVA_INT, 4); } });
        p("fill", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.fill((byte) 0x5a); return s.get(ValueLayout.JAVA_LONG, 0); } });
        p("fill.heap", () -> { byte[] b = new byte[6]; MemorySegment.ofArray(b).fill((byte) 3); return Arrays.toString(b); });
        p("fill.readonly", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).asReadOnly().fill((byte) 1); return "no"; } });
        p("set.readonly", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).asReadOnly().set(ValueLayout.JAVA_INT, 0, 1); return "no"; } });
        p("readonly.flag", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).isReadOnly() + " " + x.allocate(8).asReadOnly().isReadOnly(); } });
        p("heap.readonly.array", () -> MemorySegment.ofArray(new int[2]).asReadOnly().isReadOnly());
        p("toArray.byte", () -> { byte[] b = {1, 2, 3, 4}; return Arrays.toString(MemorySegment.ofArray(b).toArray(ValueLayout.JAVA_BYTE)); });
        p("toArray.int", () -> { int[] b = {1, 2, 3, 4}; return Arrays.toString(MemorySegment.ofArray(b).toArray(ValueLayout.JAVA_INT)); });
        p("toArray.long", () -> { long[] b = {1, 2}; return Arrays.toString(MemorySegment.ofArray(b).toArray(ValueLayout.JAVA_LONG)); });
        p("toArray.short", () -> Arrays.toString(MemorySegment.ofArray(new short[]{4, 5}).toArray(ValueLayout.JAVA_SHORT)));
        p("toArray.char", () -> Arrays.toString(MemorySegment.ofArray(new char[]{'a', 'b'}).toArray(ValueLayout.JAVA_CHAR)));
        p("toArray.float", () -> Arrays.toString(MemorySegment.ofArray(new float[]{1f, 2f}).toArray(ValueLayout.JAVA_FLOAT)));
        p("toArray.double", () -> Arrays.toString(MemorySegment.ofArray(new double[]{1d, 2d}).toArray(ValueLayout.JAVA_DOUBLE)));
        p("toArray.badsize", () -> Arrays.toString(MemorySegment.ofArray(new byte[5]).toArray(ValueLayout.JAVA_INT)));
        p("toString.heap", () -> MemorySegment.ofArray(new byte[5]).toString().replaceAll("@[0-9a-f]+", "@X"));
        p("string.get", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocateFrom("héllo"); return s.getString(0) + " " + s.byteSize(); } });
        p("string.set", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.setString(0, "abc"); return s.getString(0); } });
        p("string.set.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(3); s.setString(0, "abc"); return s.getString(0); } });
        p("string.get.nonterm", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(3); s.fill((byte) 'a'); return s.getString(0); } });
        p("equals.same", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return s.equals(s) + " " + s.equals(s.asSlice(0)) + " " + s.equals(x.allocate(8)) + " " + s.equals(null) + " " + s.equals("x"); } });
        p("equals.ofAddress", () -> MemorySegment.ofAddress(4).equals(MemorySegment.ofAddress(4)) + " " + MemorySegment.ofAddress(4).hashCode());
        p("equals.slice.ro", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return s.equals(s.asReadOnly()) + " " + (s.hashCode() == s.asReadOnly().hashCode()); } });
        p("equals.heap", () -> { byte[] b = new byte[8]; return MemorySegment.ofArray(b).equals(MemorySegment.ofArray(b)); });
        p("equals.heap.slice", () -> { byte[] b = new byte[8]; return MemorySegment.ofArray(b).equals(MemorySegment.ofArray(b).asSlice(0, 8)) + " " + MemorySegment.ofArray(b).equals(MemorySegment.ofArray(b).asSlice(1)); });
        p("asByteBuffer", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); var bb = s.asByteBuffer(); bb.putInt(0, 0x0a0b0c0d); return bb.capacity() + " " + bb.isDirect() + " " + Integer.toHexString(s.get(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0)); } });
        p("asByteBuffer.heap", () -> { byte[] b = new byte[8]; var bb = MemorySegment.ofArray(b).asByteBuffer(); bb.put(2, (byte) 9); return b[2] + " " + bb.isDirect() + " " + bb.hasArray(); });
        p("ofBuffer", () -> { var bb = java.nio.ByteBuffer.allocateDirect(16); MemorySegment s = MemorySegment.ofBuffer(bb); s.set(ValueLayout.JAVA_BYTE, 1, (byte) 5); return s.byteSize() + " " + bb.get(1) + " " + s.isNative(); });
        p("ofBuffer.heap", () -> { var bb = java.nio.ByteBuffer.wrap(new byte[16]); MemorySegment s = MemorySegment.ofBuffer(bb); s.set(ValueLayout.JAVA_BYTE, 1, (byte) 5); return s.byteSize() + " " + bb.get(1) + " " + s.isNative(); });
        p("ofBuffer.int", () -> { var bb = java.nio.IntBuffer.allocate(4); MemorySegment s = MemorySegment.ofBuffer(bb); s.set(ValueLayout.JAVA_INT, 4, 5); return s.byteSize() + " " + bb.get(1); });
        p("mismatch", () -> { byte[] x = {1, 2, 3, 4}, y = {1, 2, 9, 4}; return MemorySegment.ofArray(x).mismatch(MemorySegment.ofArray(y)) + " " + MemorySegment.ofArray(x).mismatch(MemorySegment.ofArray(x.clone())) + " " + MemorySegment.ofArray(x).mismatch(MemorySegment.ofArray(new byte[2])); });
        p("mismatch.range", () -> { byte[] x = {1, 2, 3, 4}, y = {0, 2, 3, 9}; return MemorySegment.mismatch(MemorySegment.ofArray(x), 1, 3, MemorySegment.ofArray(y), 1, 3) + " " + MemorySegment.mismatch(MemorySegment.ofArray(x), 0, 4, MemorySegment.ofArray(y), 0, 4); });
        p("segment.misc", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); return s.maxByteAlignment() >= 1 ? "ok" : "bad"; } });
        p("isAccessibleBy", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).isAccessibleBy(Thread.currentThread()) + " " + x.allocate(8).isAccessibleBy(new Thread()); } });
        p("isAccessibleBy.shared", () -> { try (Arena x = Arena.ofShared()) { return x.allocate(8).isAccessibleBy(new Thread()); } });
        p("isAccessibleBy.global", () -> Arena.global().allocate(8).isAccessibleBy(new Thread()));
        p("segment.isMapped", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).isMapped(); } });
        p("segment.isNative", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).isNative(); } });
        p("heapBase.native", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).heapBase(); } });
        p("segment.asOverlappingSlice", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); return s.asOverlappingSlice(s.asSlice(4, 4)).map(MemorySegment::byteSize) + " " + s.asSlice(0, 4).asOverlappingSlice(s.asSlice(8, 4)).isPresent(); } });
        p("scope.ofAuto.after", () -> { MemorySegment s = Arena.ofAuto().allocate(8); s.set(ValueLayout.JAVA_LONG, 0, 4L); return s.get(ValueLayout.JAVA_LONG, 0); });
    }

    static void slice() {
        p("asSlice", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); MemorySegment t = s.asSlice(8, 8); t.set(ValueLayout.JAVA_LONG, 0, 5L); return t.byteSize() + " " + s.get(ValueLayout.JAVA_LONG, 8) + " " + (t.address() - s.address()); } });
        p("asSlice.from", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(8).byteSize(); } });
        p("asSlice.oob", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(30, 8).byteSize(); } });
        p("asSlice.neg", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(-1, 8).byteSize(); } });
        p("asSlice.negLen", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(0, -1).byteSize(); } });
        p("asSlice.off.oob", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(33).byteSize(); } });
        p("asSlice.end", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).asSlice(32).byteSize(); } });
        p("asSlice.align", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32, 16).asSlice(4, 8, 4).byteSize(); } });
        p("asSlice.align.bad", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32, 16).asSlice(4, 8, 8).byteSize(); } });
        p("asSlice.layout", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32, 16).asSlice(8, ValueLayout.JAVA_LONG).byteSize(); } });
        p("heap.slice", () -> { byte[] b = new byte[16]; MemorySegment t = MemorySegment.ofArray(b).asSlice(4, 4); t.set(ValueLayout.JAVA_BYTE, 1, (byte) 8); return b[5] + " " + t.address(); });
        p("reinterpret.bad", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).reinterpret(-1).byteSize(); } });
        p("reinterpret.native", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(8).reinterpret(64).byteSize(); } });
        p("reinterpret.heap", () -> MemorySegment.ofArray(new byte[8]).reinterpret(16).byteSize());
        p("reinterpret.cleanup", () -> { int[] c = {0}; try (Arena x = Arena.ofConfined()) { MemorySegment s = MemorySegment.ofAddress(x.allocate(8).address()).reinterpret(8, x, m -> c[0]++); s.get(ValueLayout.JAVA_BYTE, 0); } return c[0]; });
        p("elements.count", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).elements(ValueLayout.JAVA_INT).count(); } });
        p("elements.sum", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); for (int i = 0; i < 8; i++) s.setAtIndex(ValueLayout.JAVA_INT, i, i + 1); return s.elements(ValueLayout.JAVA_INT).mapToInt(e -> e.get(ValueLayout.JAVA_INT, 0)).sum(); } });
        p("elements.par", () -> { try (Arena x = Arena.ofShared()) { MemorySegment s = x.allocate(4000); for (int i = 0; i < 1000; i++) s.setAtIndex(ValueLayout.JAVA_INT, i, i); return s.elements(ValueLayout.JAVA_INT).parallel().mapToLong(e -> e.get(ValueLayout.JAVA_INT, 0)).sum(); } });
        p("elements.par.confined", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(4000); return s.elements(ValueLayout.JAVA_INT).parallel().count(); } });
        p("elements.par.confined.sum", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(4000); return s.elements(ValueLayout.JAVA_INT).parallel().mapToInt(e -> 1).sum(); } });
        p("elements.badsize", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(30).elements(ValueLayout.JAVA_INT).count(); } });
        p("elements.zerolayout", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(30).elements(MemoryLayout.paddingLayout(0)).count(); } });
        p("elements.struct", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(32).elements(MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT)).count(); } });
        p("elements.spliterator", () -> { try (Arena x = Arena.ofConfined()) {
            Spliterator<MemorySegment> sp = x.allocate(40).spliterator(ValueLayout.JAVA_INT);
            return sp.characteristics() + " " + sp.estimateSize() + " " + sp.getExactSizeIfKnown() + " " + sp.hasCharacteristics(Spliterator.SIZED);
        } });
        p("elements.spliterator.split", () -> { try (Arena x = Arena.ofConfined()) {
            Spliterator<MemorySegment> sp = x.allocate(40).spliterator(ValueLayout.JAVA_INT);
            Spliterator<MemorySegment> l = sp.trySplit();
            Spliterator<MemorySegment> ll = l.trySplit();
            return l.estimateSize() + " " + sp.estimateSize() + " " + ll.estimateSize() + " " + l.estimateSize();
        } });
        p("elements.spliterator.tryAdvance", () -> { try (Arena x = Arena.ofConfined()) {
            MemorySegment s = x.allocate(12); s.setAtIndex(ValueLayout.JAVA_INT, 0, 7); s.setAtIndex(ValueLayout.JAVA_INT, 1, 8);
            Spliterator<MemorySegment> sp = s.spliterator(ValueLayout.JAVA_INT);
            StringBuilder sb = new StringBuilder();
            while (sp.tryAdvance(e -> sb.append(e.get(ValueLayout.JAVA_INT, 0)).append(','))) { }
            return sb + " " + sp.estimateSize() + " " + sp.tryAdvance(e -> { });
        } });
        p("elements.spliterator.forEachRemaining", () -> { try (Arena x = Arena.ofConfined()) {
            MemorySegment s = x.allocate(16); for (int i = 0; i < 4; i++) s.setAtIndex(ValueLayout.JAVA_INT, i, i * 3);
            Spliterator<MemorySegment> sp = s.spliterator(ValueLayout.JAVA_INT);
            sp.tryAdvance(e -> { });
            StringBuilder sb = new StringBuilder();
            sp.forEachRemaining(e -> sb.append(e.get(ValueLayout.JAVA_INT, 0)).append(','));
            return sb + " " + sp.estimateSize();
        } });
        p("elements.spliterator.split.empty", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(4).spliterator(ValueLayout.JAVA_INT).trySplit(); } });
        p("elements.spliterator.badsize", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(7).spliterator(ValueLayout.JAVA_INT).estimateSize(); } });
        p("elements.spliterator.zero", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(0).spliterator(ValueLayout.JAVA_INT).estimateSize(); } });
        p("elements.spliterator.nullconsumer", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(4).spliterator(ValueLayout.JAVA_INT).tryAdvance(null); } });
        p("elements.heap.par", () -> MemorySegment.ofArray(new int[5000]).elements(ValueLayout.JAVA_INT).parallel().count());
        p("elements.closed", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(16); Spliterator<MemorySegment> sp = s.spliterator(ValueLayout.JAVA_INT); x.close(); return sp.tryAdvance(e -> { }); });
        p("elements.spliterator.closed.create", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(16); x.close(); return s.spliterator(ValueLayout.JAVA_INT).estimateSize(); });
        p("slice.ofAuto", () -> { MemorySegment s = Arena.ofAuto().allocate(16); return s.asSlice(4, 4).byteSize(); });
    }

    static void copy() {
        p("copy.segs", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16), d = x.allocate(16); s.setAtIndex(ValueLayout.JAVA_INT, 1, 5); MemorySegment.copy(s, 0, d, 0, 16); return d.getAtIndex(ValueLayout.JAVA_INT, 1); } });
        p("copy.method", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16), d = x.allocate(16); s.setAtIndex(ValueLayout.JAVA_INT, 1, 5); d.copyFrom(s); return d.getAtIndex(ValueLayout.JAVA_INT, 1); } });
        p("copy.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(8), 0, x.allocate(8), 0, 16); return "no"; } });
        p("copy.oob.dst", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(16), 0, x.allocate(8), 4, 8); return "no"; } });
        p("copy.neg", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(16), 0, x.allocate(8), 0, -1); return "no"; } });
        p("copy.readonly", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(16), 0, x.allocate(16).asReadOnly(), 0, 4); return "no"; } });
        p("copy.overlap", () -> { byte[] b = {1, 2, 3, 4, 5, 6}; MemorySegment s = MemorySegment.ofArray(b); MemorySegment.copy(s, 0, s, 2, 4); return Arrays.toString(b); });
        p("copy.overlap2", () -> { byte[] b = {1, 2, 3, 4, 5, 6}; MemorySegment s = MemorySegment.ofArray(b); MemorySegment.copy(s, 2, s, 0, 4); return Arrays.toString(b); });
        p("copy.heap2native", () -> { try (Arena x = Arena.ofConfined()) { byte[] b = {1, 2, 3, 4}; MemorySegment d = x.allocate(4); MemorySegment.copy(MemorySegment.ofArray(b), 0, d, 0, 4); return d.get(ValueLayout.JAVA_BYTE, 3); } });
        p("copy.to.array.byte", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(4); s.set(ValueLayout.JAVA_BYTE, 2, (byte) 9); byte[] b = new byte[6]; MemorySegment.copy(s, ValueLayout.JAVA_BYTE, 0, b, 1, 4); return Arrays.toString(b); } });
        p("copy.to.array.int", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); for (int i = 0; i < 4; i++) s.setAtIndex(ValueLayout.JAVA_INT, i, i + 10); int[] b = new int[6]; MemorySegment.copy(s, ValueLayout.JAVA_INT, 4, b, 1, 3); return Arrays.toString(b); } });
        p("copy.to.array.long", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); s.setAtIndex(ValueLayout.JAVA_LONG, 1, 42); long[] b = new long[2]; MemorySegment.copy(s, ValueLayout.JAVA_LONG, 0, b, 0, 2); return Arrays.toString(b); } });
        p("copy.to.array.short", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.setAtIndex(ValueLayout.JAVA_SHORT, 1, (short) 4); short[] b = new short[4]; MemorySegment.copy(s, ValueLayout.JAVA_SHORT, 0, b, 0, 4); return Arrays.toString(b); } });
        p("copy.to.array.char", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.setAtIndex(ValueLayout.JAVA_CHAR, 1, 'z'); char[] b = new char[4]; MemorySegment.copy(s, ValueLayout.JAVA_CHAR, 0, b, 0, 4); return Arrays.toString(b); } });
        p("copy.to.array.float", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.setAtIndex(ValueLayout.JAVA_FLOAT, 1, 2.5f); float[] b = new float[2]; MemorySegment.copy(s, ValueLayout.JAVA_FLOAT, 0, b, 0, 2); return Arrays.toString(b); } });
        p("copy.to.array.double", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); s.setAtIndex(ValueLayout.JAVA_DOUBLE, 1, 2.5); double[] b = new double[2]; MemorySegment.copy(s, ValueLayout.JAVA_DOUBLE, 0, b, 0, 2); return Arrays.toString(b); } });
        p("copy.from.array.int", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment d = x.allocate(16); MemorySegment.copy(new int[]{9, 8, 7, 6}, 1, d, ValueLayout.JAVA_INT, 0, 3); return d.getAtIndex(ValueLayout.JAVA_INT, 0) + " " + d.getAtIndex(ValueLayout.JAVA_INT, 2); } });
        p("copy.from.array.bytes", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment d = x.allocate(4); MemorySegment.copy(new byte[]{9, 8, 7, 6}, 1, d, ValueLayout.JAVA_BYTE, 0, 3); return d.get(ValueLayout.JAVA_BYTE, 2); } });
        p("copy.from.array.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(new int[2], 0, x.allocate(16), ValueLayout.JAVA_INT, 0, 3); return "no"; } });
        p("copy.to.array.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(4), ValueLayout.JAVA_INT, 0, new int[2], 0, 3); return "no"; } });
        p("copy.to.array.badtype", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(8), ValueLayout.JAVA_INT, 0, new long[2], 0, 2); return "no"; } });
        p("copy.to.notarray", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(8), ValueLayout.JAVA_INT, 0, "s", 0, 2); return "no"; } });
        p("copy.to.array.swap", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 0, 0x01020304); int[] b = new int[2]; MemorySegment.copy(s, ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0, b, 0, 2); return Integer.toHexString(b[0]); } });
        p("copy.layout.mismatch", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(8), ValueLayout.JAVA_INT, 0, x.allocate(8), ValueLayout.JAVA_LONG, 0, 1); return "no"; } });
        p("copy.layouts", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.setAtIndex(ValueLayout.JAVA_INT, 1, 3); MemorySegment d = x.allocate(8); MemorySegment.copy(s, ValueLayout.JAVA_INT, 0, d, ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0, 2); return d.getAtIndex(ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 1); } });
        p("copy.bytes.unaligned", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment d = x.allocate(16); MemorySegment.copy(new int[]{1, 2, 3}, 0, d, ValueLayout.JAVA_INT_UNALIGNED, 1, 3); return d.get(ValueLayout.JAVA_INT_UNALIGNED, 5); } });
        p("copyFrom.size", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).copyFrom(x.allocate(16)); return "no"; } });
        p("copyFrom.ret", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment d = x.allocate(8); return d.copyFrom(x.allocate(8)) == d; } });
        p("copy.closed.src", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); MemorySegment d = Arena.ofAuto().allocate(8); x.close(); MemorySegment.copy(s, 0, d, 0, 8); return "no"; });
        p("copy.closed.dst", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); MemorySegment d = Arena.ofAuto().allocate(8); x.close(); MemorySegment.copy(d, 0, s, 0, 8); return "no"; });
        p("copy.closed.array", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); MemorySegment.copy(s, ValueLayout.JAVA_BYTE, 0, new byte[8], 0, 8); return "no"; });
        p("copy.large", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(1 << 16), d = x.allocate(1 << 16); s.fill((byte) 7); MemorySegment.copy(s, 0, d, 0, 1 << 16); return d.get(ValueLayout.JAVA_BYTE, (1 << 16) - 1) + " " + s.mismatch(d); } });
        p("copy.selfsame", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_LONG, 0, 99L); MemorySegment.copy(s, 0, s, 0, 8); return s.get(ValueLayout.JAVA_LONG, 0); } });
        p("copy.zero", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment.copy(x.allocate(8), 0, x.allocate(8), 0, 0); return "ok"; } });
    }

    static void closed() {
        p("closed.get", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.get(ValueLayout.JAVA_INT, 0); });
        p("closed.set", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); s.set(ValueLayout.JAVA_INT, 0, 1); return "no"; });
        p("closed.fill", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); s.fill((byte) 0); return "no"; });
        p("closed.slice", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.asSlice(0, 4).byteSize(); });
        p("closed.slice.get", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8).asSlice(0, 4); x.close(); return s.get(ValueLayout.JAVA_BYTE, 0); });
        p("closed.byteSize", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.byteSize(); });
        p("closed.allocate", () -> { Arena x = Arena.ofConfined(); x.close(); return x.allocate(8).byteSize(); });
        p("closed.allocateFrom", () -> { Arena x = Arena.ofConfined(); x.close(); return x.allocateFrom("s").byteSize(); });
        p("closed.asByteBuffer", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.asByteBuffer().capacity(); });
        p("closed.bb.get", () -> { Arena x = Arena.ofConfined(); var bb = x.allocate(8).asByteBuffer(); x.close(); return bb.get(0); });
        p("closed.bb.put", () -> { Arena x = Arena.ofConfined(); var bb = x.allocate(8).asByteBuffer(); x.close(); bb.put(0, (byte) 1); return "no"; });
        p("closed.toArray", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.toArray(ValueLayout.JAVA_BYTE).length; });
        p("closed.getString", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.getString(0); });
        p("closed.mismatch", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.mismatch(MemorySegment.ofArray(new byte[8])); });
        p("closed.reinterpret", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.reinterpret(16).byteSize(); });
        p("closed.equals", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.equals(s); });
        p("closed.address", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.address() != 0; });
        p("closed.getAtIndex", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.getAtIndex(ValueLayout.JAVA_INT, 0); });
        p("closed.shared.get", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); x.close(); return s.get(ValueLayout.JAVA_INT, 0); });
        p("closed.shared.set", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); x.close(); s.set(ValueLayout.JAVA_INT, 0, 1); return "no"; });
        p("closed.shared.fill", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); x.close(); s.fill((byte) 0); return "no"; });
        p("closed.shared.allocate", () -> { Arena x = Arena.ofShared(); x.close(); return x.allocate(8); });
        p("closed.shared.copy", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); x.close(); MemorySegment.copy(s, 0, MemorySegment.ofArray(new byte[8]), 0, 8); return "no"; });
        p("closed.elements", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); x.close(); return s.elements(ValueLayout.JAVA_INT).count(); });
        p("closed.spl.tryAdvance", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); var sp = s.spliterator(ValueLayout.JAVA_INT); x.close(); return sp.tryAdvance(e -> { }); });
        p("closed.spl.forEach", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); var sp = s.spliterator(ValueLayout.JAVA_INT); x.close(); sp.forEachRemaining(e -> { }); return "no"; });
        p("closed.vh.get", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); x.close(); return (int) vh.get(s, 0L); });
        p("closed.vh.set", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); x.close(); vh.set(s, 0L, 1); return "no"; });
        p("closed.vh.cas", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(8); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); x.close(); return vh.compareAndSet(s, 0L, 0, 1); });
        p("shared.close.during.use", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 0, 3); int v = s.get(ValueLayout.JAVA_INT, 0); x.close(); return v; });
        p("closed.auto.close", () -> { Arena.ofAuto().close(); return "no"; });
        p("try.with.resources.throw", () -> { MemorySegment[] keep = new MemorySegment[1]; try (Arena x = Arena.ofConfined()) { keep[0] = x.allocate(8); throw new IllegalStateException("boom"); } catch (IllegalStateException e) { return e.getMessage() + " " + keep[0].scope().isAlive(); } });
    }

    static void thread() {
        p("thread.get", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return onOther(() -> s.get(ValueLayout.JAVA_INT, 0)); } });
        p("thread.set", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return onOther(() -> { s.set(ValueLayout.JAVA_INT, 0, 1); return "no"; }); } });
        p("thread.slice", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return onOther(() -> s.asSlice(0, 4).byteSize()); } });
        p("thread.close", () -> { Arena x = Arena.ofConfined(); return onOther(() -> { x.close(); return "no"; }); });
        p("thread.allocate", () -> { Arena x = Arena.ofConfined(); return onOther(() -> x.allocate(8).byteSize()); });
        p("thread.fill", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return onOther(() -> { s.fill((byte) 1); return "no"; }); } });
        p("thread.copy", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); return onOther(() -> { MemorySegment.copy(s, 0, MemorySegment.ofArray(new byte[8]), 0, 8); return "no"; }); } });
        p("thread.vh", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); return onOther(() -> (int) vh.get(s, 0L)); } });
        p("thread.scope.alive", () -> { try (Arena x = Arena.ofConfined()) { return onOther(() -> x.scope().isAlive()); } });
        p("thread.bb", () -> { try (Arena x = Arena.ofConfined()) { var bb = x.allocate(8).asByteBuffer(); return onOther(() -> bb.get(0)); } });
        p("thread.shared.get", () -> { try (Arena x = Arena.ofShared()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 0, 6); return onOther(() -> s.get(ValueLayout.JAVA_INT, 0)); } });
        p("thread.shared.close", () -> { Arena x = Arena.ofShared(); MemorySegment s = x.allocate(8); String r = String.valueOf(onOther(() -> { x.close(); return "closed"; })); return r + " " + x.scope().isAlive(); });
        p("thread.global.get", () -> { MemorySegment s = Arena.global().allocate(8); return onOther(() -> s.get(ValueLayout.JAVA_INT, 0)); });
        p("thread.global.after", () -> { Arena x = Arena.ofConfined(); MemorySegment[] s = new MemorySegment[1]; onOther(() -> { Arena y = Arena.ofConfined(); s[0] = y.allocate(4); s[0].set(ValueLayout.JAVA_INT, 0, 4); y.close(); return "ok"; }); return s[0].scope().isAlive(); });
        p("thread.spliterator", () -> { try (Arena x = Arena.ofConfined()) { var sp = x.allocate(8).spliterator(ValueLayout.JAVA_INT); return onOther(() -> sp.tryAdvance(e -> { })); } });
        p("thread.elements.stream", () -> { try (Arena x = Arena.ofConfined()) { var st = x.allocate(8).elements(ValueLayout.JAVA_INT); return onOther(() -> st.count()); } });
        p("thread.heap", () -> { MemorySegment s = MemorySegment.ofArray(new int[2]); return onOther(() -> s.get(ValueLayout.JAVA_INT, 0)); });
    }

    static Object onOther(java.util.concurrent.Callable<Object> c) {
        Object[] r = new Object[1];
        Thread t = new Thread(() -> {
            try { r[0] = c.call(); } catch (Throwable e) { r[0] = "THREW " + e.getClass().getName() + ": " + e.getMessage(); }
        });
        t.start();
        try { t.join(); } catch (InterruptedException e) { }
        return r[0];
    }

    static void layout() {
        StructLayout pt = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y")).withName("Point");
        p("struct.toString", () -> pt);
        p("struct.size", () -> pt.byteSize() + " " + pt.byteAlignment() + " " + pt.name());
        p("struct.offset", () -> pt.byteOffset(MemoryLayout.PathElement.groupElement("y")));
        p("struct.offset.bad", () -> pt.byteOffset(MemoryLayout.PathElement.groupElement("z")));
        p("struct.select", () -> pt.select(MemoryLayout.PathElement.groupElement("x")));
        p("struct.members", () -> pt.memberLayouts());
        p("struct.padding", () -> MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_INT));
        p("struct.padding.ok", () -> MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, MemoryLayout.paddingLayout(3), ValueLayout.JAVA_INT).byteSize());
        p("struct.empty", () -> MemoryLayout.structLayout().byteSize());
        p("struct.null", () -> MemoryLayout.structLayout((MemoryLayout[]) null));
        p("struct.nullelem", () -> MemoryLayout.structLayout(ValueLayout.JAVA_INT, null));
        p("struct.withByteAlignment", () -> pt.withByteAlignment(16).byteAlignment());
        p("struct.withByteAlignment.bad", () -> pt.withByteAlignment(2));
        p("struct.withByteAlignment.bad3", () -> pt.withByteAlignment(3));
        p("struct.withName", () -> pt.withName("P2").name() + " " + pt.name());
        p("struct.withoutName", () -> pt.withoutName().name());
        p("struct.equals", () -> pt.equals(MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y")).withName("Point")) + " " + pt.equals(pt.withName("Q")) + " " + pt.equals(null) + " " + pt.equals("x"));
        p("struct.hash", () -> pt.hashCode() == MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y")).withName("Point").hashCode());
        p("union.size", () -> MemoryLayout.unionLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteSize() + " " + MemoryLayout.unionLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG));
        p("union.empty", () -> MemoryLayout.unionLayout().byteSize());
        p("union.offset", () -> MemoryLayout.unionLayout(ValueLayout.JAVA_INT.withName("i"), ValueLayout.JAVA_LONG.withName("l")).byteOffset(MemoryLayout.PathElement.groupElement("l")));
        p("union.null", () -> MemoryLayout.unionLayout((MemoryLayout[]) null));
        p("seq.of", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT));
        p("seq.size", () -> { SequenceLayout s = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT); return s.byteSize() + " " + s.elementCount() + " " + s.elementLayout(); });
        p("seq.neg", () -> MemoryLayout.sequenceLayout(-1, ValueLayout.JAVA_INT));
        p("seq.zero", () -> MemoryLayout.sequenceLayout(0, ValueLayout.JAVA_INT).byteSize());
        p("seq.overflow", () -> MemoryLayout.sequenceLayout(Long.MAX_VALUE, ValueLayout.JAVA_INT));
        p("seq.big", () -> MemoryLayout.sequenceLayout(1L << 40, ValueLayout.JAVA_INT).byteSize());
        p("seq.null", () -> MemoryLayout.sequenceLayout(2, null));
        p("seq.withCount", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).withElementCount(8));
        p("seq.withCount.neg", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).withElementCount(-1));
        p("seq.reshape", () -> MemoryLayout.sequenceLayout(12, ValueLayout.JAVA_INT).reshape(3, 4));
        p("seq.reshape.infer", () -> MemoryLayout.sequenceLayout(12, ValueLayout.JAVA_INT).reshape(-1, 4));
        p("seq.reshape.bad", () -> MemoryLayout.sequenceLayout(12, ValueLayout.JAVA_INT).reshape(5, 4));
        p("seq.flatten", () -> MemoryLayout.sequenceLayout(3, MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)).flatten());
        p("seq.flatten.plain", () -> MemoryLayout.sequenceLayout(3, ValueLayout.JAVA_INT).flatten());
        p("seq.offset", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteOffset(MemoryLayout.PathElement.sequenceElement(2)));
        p("seq.offset.oob", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteOffset(MemoryLayout.PathElement.sequenceElement(4)));
        p("seq.offset.range", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteOffset(MemoryLayout.PathElement.sequenceElement(1, 2)));
        p("seq.select", () -> MemoryLayout.sequenceLayout(4, pt).select(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")));
        p("seq.select.badpath", () -> MemoryLayout.sequenceLayout(4, pt).select(MemoryLayout.PathElement.groupElement("y")));
        p("seq.byteOffsetHandle", () -> (long) MemoryLayout.sequenceLayout(4, pt).byteOffsetHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")).invokeExact(2L));
        p("seq.byteOffsetHandle.type", () -> MemoryLayout.sequenceLayout(4, pt).byteOffsetHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")).type());
        p("seq.byteOffsetHandle.oob", () -> (long) MemoryLayout.sequenceLayout(4, pt).byteOffsetHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")).invokeExact(4L));
        p("seq.sliceHandle", () -> MemoryLayout.sequenceLayout(4, pt).sliceHandle(MemoryLayout.PathElement.sequenceElement()).type());
        p("seq.sliceHandle.call", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(MemoryLayout.sequenceLayout(4, pt)); return ((MemorySegment) MemoryLayout.sequenceLayout(4, pt).sliceHandle(MemoryLayout.PathElement.sequenceElement()).invokeExact(s, 0L, 2L)).byteSize(); } });
        p("seq.sliceHandle.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(MemoryLayout.sequenceLayout(4, pt)); return ((MemorySegment) MemoryLayout.sequenceLayout(4, pt).sliceHandle(MemoryLayout.PathElement.sequenceElement()).invokeExact(s, 0L, 4L)).byteSize(); } });
        p("pad.of", () -> MemoryLayout.paddingLayout(4));
        p("pad.size", () -> MemoryLayout.paddingLayout(4).byteSize() + " " + MemoryLayout.paddingLayout(4).byteAlignment());
        p("pad.zero", () -> MemoryLayout.paddingLayout(0));
        p("pad.neg", () -> MemoryLayout.paddingLayout(-1));
        p("pad.withName", () -> MemoryLayout.paddingLayout(4).withName("p"));
        p("pad.withAlign", () -> MemoryLayout.paddingLayout(4).withByteAlignment(2));
        p("value.toString", () -> ValueLayout.JAVA_INT + " " + ValueLayout.JAVA_LONG + " " + ValueLayout.JAVA_BYTE + " " + ValueLayout.JAVA_SHORT + " " + ValueLayout.JAVA_CHAR + " " + ValueLayout.JAVA_FLOAT + " " + ValueLayout.JAVA_DOUBLE + " " + ValueLayout.JAVA_BOOLEAN + " " + ValueLayout.ADDRESS);
        p("value.unaligned", () -> ValueLayout.JAVA_INT_UNALIGNED + " " + ValueLayout.JAVA_LONG_UNALIGNED + " " + ValueLayout.ADDRESS_UNALIGNED);
        p("value.order", () -> ValueLayout.JAVA_INT.order() + " " + ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN));
        p("value.withName", () -> ValueLayout.JAVA_INT.withName("a") + " " + ValueLayout.JAVA_INT.withName("a").equals(ValueLayout.JAVA_INT) + " " + ValueLayout.JAVA_INT.withName("a").equals(ValueLayout.JAVA_INT.withName("a")));
        p("value.carrier", () -> ValueLayout.JAVA_INT.carrier() + " " + ValueLayout.ADDRESS.carrier() + " " + ValueLayout.JAVA_BOOLEAN.carrier());
        p("value.bad.align", () -> ValueLayout.JAVA_INT.withByteAlignment(3));
        p("value.bad.align2", () -> ValueLayout.JAVA_INT.withByteAlignment(0));
        p("value.big.align", () -> ValueLayout.JAVA_INT.withByteAlignment(1L << 40).byteAlignment());
        p("value.neg.align", () -> ValueLayout.JAVA_INT.withByteAlignment(-4));
        p("address.target", () -> ValueLayout.ADDRESS.withTargetLayout(MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)).targetLayout());
        p("address.target.none", () -> ValueLayout.ADDRESS.targetLayout());
        p("address.target.get", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment p = x.allocate(8); MemorySegment d = x.allocate(16); p.set(ValueLayout.ADDRESS, 0, d); return p.get(ValueLayout.ADDRESS.withTargetLayout(MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)), 0).byteSize() + " " + p.get(ValueLayout.ADDRESS, 0).byteSize(); } });
        p("address.target.toString", () -> ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT));
        p("address.withoutTarget", () -> ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT).withoutTargetLayout());
        p("layout.nested", () -> MemoryLayout.structLayout(ValueLayout.JAVA_LONG.withName("a"), MemoryLayout.unionLayout(ValueLayout.JAVA_INT.withName("i"), ValueLayout.JAVA_FLOAT.withName("f")).withName("u"), MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_SHORT).withName("s")));
        p("layout.nested.offset", () -> MemoryLayout.structLayout(ValueLayout.JAVA_LONG.withName("a"), MemoryLayout.unionLayout(ValueLayout.JAVA_INT.withName("i"), ValueLayout.JAVA_FLOAT.withName("f")).withName("u"), MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_SHORT).withName("s")).byteOffset(MemoryLayout.PathElement.groupElement("s"), MemoryLayout.PathElement.sequenceElement(1)));
        p("layout.dup.name", () -> MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("a"), ValueLayout.JAVA_INT.withName("a")).byteOffset(MemoryLayout.PathElement.groupElement("a")));
        p("layout.path.idx", () -> MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteOffset(MemoryLayout.PathElement.groupElement(1)));
        p("layout.path.idx.oob", () -> MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteOffset(MemoryLayout.PathElement.groupElement(3)));
        p("layout.path.neg", () -> MemoryLayout.PathElement.sequenceElement(-1));
        p("layout.path.dereference", () -> MemoryLayout.structLayout(ValueLayout.ADDRESS.withTargetLayout(pt).withName("p")).select(MemoryLayout.PathElement.groupElement("p"), MemoryLayout.PathElement.dereferenceElement(), MemoryLayout.PathElement.groupElement("y")));
        p("layout.path.nullname", () -> MemoryLayout.PathElement.groupElement(null));
        p("layout.path.badpath.value", () -> ValueLayout.JAVA_INT.byteOffset(MemoryLayout.PathElement.groupElement("x")));
        p("layout.path.empty", () -> ValueLayout.JAVA_INT.byteOffset());
        p("layout.select.empty", () -> ValueLayout.JAVA_INT.select());
        p("layout.value.varhandle.path", () -> ValueLayout.JAVA_INT.varHandle(MemoryLayout.PathElement.groupElement("x")));
        p("layout.scale", () -> ValueLayout.JAVA_INT.scale(4, 3));
        p("layout.scale.neg", () -> ValueLayout.JAVA_INT.scale(4, -3));
        p("layout.scale.overflow", () -> ValueLayout.JAVA_LONG.scale(0, Long.MAX_VALUE));
        p("layout.hash.value", () -> ValueLayout.JAVA_INT.hashCode() == ValueLayout.JAVA_INT.withName("z").withoutName().hashCode());
        p("layout.sameAs", () -> ValueLayout.JAVA_INT.withByteAlignment(2).equals(ValueLayout.JAVA_INT_UNALIGNED.withByteAlignment(2)) + " " + ValueLayout.JAVA_INT.equals(ValueLayout.JAVA_INT_UNALIGNED));
        p("layout.class.iface", () -> ValueLayout.JAVA_INT.getClass().isInterface() + " " + pt.getClass().isInterface() + " " + MemoryLayout.paddingLayout(1).getClass().isInterface());
        p("layout.instanceof", () -> (ValueLayout.JAVA_INT instanceof ValueLayout.OfInt) + " " + (pt instanceof GroupLayout) + " " + (MemoryLayout.unionLayout() instanceof UnionLayout) + " " + (MemoryLayout.sequenceLayout(1, pt) instanceof SequenceLayout));
        p("layout.sizes", () -> ValueLayout.ADDRESS.byteSize() + " " + ValueLayout.JAVA_CHAR.byteSize() + " " + ValueLayout.JAVA_BOOLEAN.byteSize());
        p("layout.align.struct", () -> MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, MemoryLayout.paddingLayout(7), ValueLayout.JAVA_LONG).byteAlignment());
        p("layout.struct.mis", () -> MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_INT.withByteAlignment(1)).byteSize());
        p("layout.union.align", () -> MemoryLayout.unionLayout(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_LONG).byteAlignment());
        p("layout.allocate.struct", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(pt).byteSize() + " " + x.allocate(pt, 3).byteSize(); } });
        p("layout.allocate.seq", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(MemoryLayout.sequenceLayout(3, pt)).byteSize(); } });
        p("layout.allocate.arr", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(pt, 3).byteSize(); } });
        p("layout.allocate.neg", () -> { try (Arena x = Arena.ofConfined()) { return x.allocate(pt, -1).byteSize(); } });
        p("layout.allocateFrom.arr", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_LONG, new long[]{1, 2, 3}).byteSize(); } });
        p("layout.allocateFrom.short", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_SHORT, new short[]{1, 2, 3}).byteSize(); } });
        p("layout.allocateFrom.char", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_CHAR, new char[]{1, 2, 3}).byteSize(); } });
        p("layout.allocateFrom.float", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_FLOAT, new float[]{1, 2, 3}).byteSize(); } });
        p("layout.allocateFrom.double", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_DOUBLE, new double[]{1, 2, 3}).byteSize(); } });
        p("layout.allocateFrom.addr", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.ADDRESS, MemorySegment.ofAddress(5)).get(ValueLayout.ADDRESS, 0).address(); } });
        p("layout.allocateFrom.byte", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom(ValueLayout.JAVA_BYTE, (byte) 7).byteSize(); } });
        p("layout.allocateFrom.str.utf16", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom("ab", java.nio.charset.StandardCharsets.UTF_16LE).byteSize(); } });
        p("layout.allocateFrom.str.nul", () -> { try (Arena x = Arena.ofConfined()) { return x.allocateFrom("a\0b").byteSize(); } });
    }

    static void vh() {
        StructLayout pt = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y"), ValueLayout.JAVA_LONG.withName("z"));
        SequenceLayout arr = MemoryLayout.sequenceLayout(4, pt);
        p("vh.int.type", () -> ValueLayout.JAVA_INT.varHandle().varType() + " " + ValueLayout.JAVA_INT.varHandle().coordinateTypes());
        p("vh.all.types", () -> ValueLayout.JAVA_BYTE.varHandle().varType() + " " + ValueLayout.JAVA_SHORT.varHandle().varType() + " " + ValueLayout.JAVA_CHAR.varHandle().varType() + " " + ValueLayout.JAVA_LONG.varHandle().varType() + " " + ValueLayout.JAVA_FLOAT.varHandle().varType() + " " + ValueLayout.JAVA_DOUBLE.varHandle().varType() + " " + ValueLayout.JAVA_BOOLEAN.varHandle().varType() + " " + ValueLayout.ADDRESS.varHandle().varType());
        p("vh.int.getset", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.set(s, 4L, 77); return (int) vh.get(s, 4L) + " " + s.get(ValueLayout.JAVA_INT, 4); } });
        p("vh.byte", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_BYTE.varHandle(); vh.set(s, 4L, (byte) 7); return (byte) vh.get(s, 4L); } });
        p("vh.short", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_SHORT.varHandle(); vh.set(s, 4L, (short) 7); return (short) vh.get(s, 4L); } });
        p("vh.char", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_CHAR.varHandle(); vh.set(s, 4L, 'c'); return (char) vh.get(s, 4L); } });
        p("vh.long", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_LONG.varHandle(); vh.set(s, 8L, 1L << 40); return (long) vh.get(s, 8L); } });
        p("vh.float", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_FLOAT.varHandle(); vh.set(s, 4L, 1.5f); return (float) vh.get(s, 4L); } });
        p("vh.double", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_DOUBLE.varHandle(); vh.set(s, 8L, 1.5); return (double) vh.get(s, 8L); } });
        p("vh.boolean", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_BOOLEAN.varHandle(); vh.set(s, 1L, true); return (boolean) vh.get(s, 1L); } });
        p("vh.address", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.ADDRESS.varHandle(); vh.set(s, 8L, MemorySegment.ofAddress(123)); return ((MemorySegment) vh.get(s, 8L)).address(); } });
        p("vh.oob", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.varHandle().get(x.allocate(8), 8L); } });
        p("vh.oob.set", () -> { try (Arena x = Arena.ofConfined()) { ValueLayout.JAVA_INT.varHandle().set(x.allocate(8), 8L, 1); return "no"; } });
        p("vh.neg", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.varHandle().get(x.allocate(8), -4L); } });
        p("vh.misaligned", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.varHandle().get(x.allocate(16, 8), 2L); } });
        p("vh.unaligned.ok", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT_UNALIGNED.varHandle().get(x.allocate(16, 8), 2L); } });
        p("vh.readonly.set", () -> { try (Arena x = Arena.ofConfined()) { ValueLayout.JAVA_INT.varHandle().set(x.allocate(8).asReadOnly(), 0L, 1); return "no"; } });
        p("vh.readonly.get", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.varHandle().get(x.allocate(8).asReadOnly(), 0L); } });
        p("vh.null.seg", () -> (int) ValueLayout.JAVA_INT.varHandle().get((MemorySegment) null, 0L));
        p("vh.heap", () -> { int[] b = new int[4]; MemorySegment s = MemorySegment.ofArray(b); ValueLayout.JAVA_INT.varHandle().set(s, 4L, 5); return b[1]; });
        p("vh.cas", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.set(s, 0L, 5); return vh.compareAndSet(s, 0L, 5, 6) + " " + vh.compareAndSet(s, 0L, 5, 7) + " " + (int) vh.get(s, 0L); } });
        p("vh.cas.long", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_LONG.varHandle(); vh.set(s, 0L, 5L); return vh.compareAndSet(s, 0L, 5L, 6L) + " " + (long) vh.get(s, 0L); } });
        p("vh.getAndAdd", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.set(s, 0L, 5); return (int) vh.getAndAdd(s, 0L, 3) + " " + (int) vh.get(s, 0L); } });
        p("vh.getAndAdd.long", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_LONG.varHandle(); vh.set(s, 0L, 5L); return (long) vh.getAndAdd(s, 0L, 3L) + " " + (long) vh.get(s, 0L); } });
        p("vh.getAndSet", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.set(s, 0L, 5); return (int) vh.getAndSet(s, 0L, 9) + " " + (int) vh.get(s, 0L); } });
        p("vh.getAndBitwiseOr", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.set(s, 0L, 5); return (int) vh.getAndBitwiseOr(s, 0L, 2) + " " + (int) vh.get(s, 0L); } });
        p("vh.volatile", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_INT.varHandle(); vh.setVolatile(s, 0L, 5); return (int) vh.getVolatile(s, 0L) + " " + (int) vh.getAcquire(s, 0L) + " " + (int) vh.getOpaque(s, 0L); } });
        p("vh.cas.float", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_FLOAT.varHandle(); vh.set(s, 0L, 1.5f); return vh.compareAndSet(s, 0L, 1.5f, 2.5f) + " " + (float) vh.get(s, 0L); } });
        p("vh.cas.byte", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_BYTE.varHandle(); vh.set(s, 0L, (byte) 1); return vh.compareAndSet(s, 0L, (byte) 1, (byte) 2) + " " + (byte) vh.get(s, 0L); } });
        p("vh.cas.short", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle vh = ValueLayout.JAVA_SHORT.varHandle(); vh.set(s, 0L, (short) 1); return vh.compareAndSet(s, 0L, (short) 1, (short) 2) + " " + (short) vh.get(s, 0L); } });
        p("vh.bigendian", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN).varHandle().set(s, 0L, 0x01020304); return s.get(ValueLayout.JAVA_BYTE, 0); } });
        p("vh.wrongtype", () -> { try (Arena x = Arena.ofConfined()) { ValueLayout.JAVA_INT.varHandle().set(x.allocate(8), 0L, "s"); return "no"; } });
        p("vh.wrongarity", () -> { try (Arena x = Arena.ofConfined()) { ValueLayout.JAVA_INT.varHandle().set(x.allocate(8), 1); return "no"; } });
        p("vh.struct.x", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(pt); VarHandle h = pt.varHandle(MemoryLayout.PathElement.groupElement("y")); h.set(s, 0L, 88); return (int) h.get(s, 0L) + " " + s.get(ValueLayout.JAVA_INT, 4) + " " + h.coordinateTypes() + " " + h.varType(); } });
        p("vh.struct.z", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(pt); VarHandle h = pt.varHandle(MemoryLayout.PathElement.groupElement("z")); h.set(s, 0L, 88L); return (long) h.get(s, 0L) + " " + s.get(ValueLayout.JAVA_LONG, 8); } });
        p("vh.struct.badname", () -> pt.varHandle(MemoryLayout.PathElement.groupElement("q")));
        p("vh.struct.nopath", () -> pt.varHandle());
        p("vh.struct.nullpath", () -> pt.varHandle((MemoryLayout.PathElement[]) null));
        p("vh.struct.group", () -> pt.varHandle(MemoryLayout.PathElement.groupElement(0)).varType());
        p("vh.seq.idx", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")); h.set(s, 0L, 2L, 99); return (int) h.get(s, 0L, 2L) + " " + s.get(ValueLayout.JAVA_INT, 2 * 16 + 4) + " " + h.coordinateTypes(); } });
        p("vh.seq.idx.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")); return (int) h.get(s, 0L, 4L); } });
        p("vh.seq.idx.neg", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("y")); return (int) h.get(s, 0L, -1L); } });
        p("vh.seq.fixed", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(3), MemoryLayout.PathElement.groupElement("x")); h.set(s, 0L, 5); return (int) h.get(s, 0L) + " " + s.get(ValueLayout.JAVA_INT, 48) + " " + h.coordinateTypes(); } });
        p("vh.seq.fixed.oob", () -> arr.varHandle(MemoryLayout.PathElement.sequenceElement(4), MemoryLayout.PathElement.groupElement("x")));
        p("vh.seq.range", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(1, 2), MemoryLayout.PathElement.groupElement("x")); h.set(s, 0L, 1L, 6); return s.get(ValueLayout.JAVA_INT, 48) + " " + h.coordinateTypes(); } });
        p("vh.seq.range.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(1, 2), MemoryLayout.PathElement.groupElement("x")); return (int) h.get(s, 0L, 2L); } });
        p("vh.seq.twodim", () -> { SequenceLayout m = MemoryLayout.sequenceLayout(3, MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)); try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(m); VarHandle h = m.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.sequenceElement()); h.set(s, 0L, 2L, 3L, 4); return (int) h.get(s, 0L, 2L, 3L) + " " + s.get(ValueLayout.JAVA_INT, 44) + " " + h.coordinateTypes(); } });
        p("vh.seq.twodim.oob", () -> { SequenceLayout m = MemoryLayout.sequenceLayout(3, MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)); try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(m); VarHandle h = m.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.sequenceElement()); return (int) h.get(s, 0L, 0L, 4L); } });
        p("vh.seq.cas", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("x")); h.set(s, 0L, 1L, 5); return h.compareAndSet(s, 0L, 1L, 5, 6) + " " + (int) h.get(s, 0L, 1L); } });
        p("vh.seq.getAndAdd", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr); VarHandle h = arr.varHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("z")); h.set(s, 0L, 1L, 5L); return (long) h.getAndAdd(s, 0L, 1L, 2L) + " " + (long) h.get(s, 0L, 1L); } });
        p("vh.union", () -> { UnionLayout u = MemoryLayout.unionLayout(ValueLayout.JAVA_INT.withName("i"), ValueLayout.JAVA_FLOAT.withName("f")); try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(u); u.varHandle(MemoryLayout.PathElement.groupElement("f")).set(s, 0L, 1.0f); return (int) u.varHandle(MemoryLayout.PathElement.groupElement("i")).get(s, 0L); } });
        p("vh.nested", () -> { StructLayout o = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("a"), pt.withName("p")); try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(o); VarHandle h = o.varHandle(MemoryLayout.PathElement.groupElement("p"), MemoryLayout.PathElement.groupElement("z")); h.set(s, 0L, 55L); return (long) h.get(s, 0L) + " " + s.get(ValueLayout.JAVA_LONG, 16) + " " + h.coordinateTypes(); } });
        p("vh.deref", () -> { StructLayout o = MemoryLayout.structLayout(ValueLayout.ADDRESS.withTargetLayout(pt).withName("p")); try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(o); MemorySegment t = x.allocate(pt); s.set(ValueLayout.ADDRESS, 0, t); VarHandle h = o.varHandle(MemoryLayout.PathElement.groupElement("p"), MemoryLayout.PathElement.dereferenceElement(), MemoryLayout.PathElement.groupElement("y")); h.set(s, 0L, 12); return (int) h.get(s, 0L) + " " + t.get(ValueLayout.JAVA_INT, 4) + " " + h.coordinateTypes(); } });
        p("vh.deref.notarget", () -> { StructLayout o = MemoryLayout.structLayout(ValueLayout.ADDRESS.withName("p")); return o.varHandle(MemoryLayout.PathElement.groupElement("p"), MemoryLayout.PathElement.dereferenceElement()); });
        p("vh.arrayElement.int", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); h.set(s, 0L, 2L, 77); return (int) h.get(s, 0L, 2L) + " " + s.get(ValueLayout.JAVA_INT, 8) + " " + h.coordinateTypes(); } });
        p("vh.arrayElement.int.oob", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.arrayElementVarHandle().get(x.allocate(16), 0L, 4L); } });
        p("vh.arrayElement.long", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_LONG.arrayElementVarHandle(); h.set(s, 0L, 3L, 5L); return (long) h.get(s, 0L, 3L) + " " + s.get(ValueLayout.JAVA_LONG, 24); } });
        p("vh.arrayElement.byte", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_BYTE.arrayElementVarHandle(); h.set(s, 0L, 3L, (byte) 5); return (byte) h.get(s, 0L, 3L); } });
        p("vh.arrayElement.double", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_DOUBLE.arrayElementVarHandle(); h.set(s, 0L, 3L, 5.5); return (double) h.get(s, 0L, 3L); } });
        p("vh.arrayElement.cas", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); h.set(s, 0L, 3L, 5); return h.compareAndSet(s, 0L, 3L, 5, 6) + " " + (int) h.get(s, 0L, 3L); } });
        p("vh.arrayElement.getAndAdd", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); h.set(s, 0L, 3L, 5); return (int) h.getAndAdd(s, 0L, 3L, 4) + " " + (int) h.get(s, 0L, 3L); } });
        p("vh.arrayElement.struct", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(pt, 3); VarHandle h = pt.arrayElementVarHandle(MemoryLayout.PathElement.groupElement("z")); h.set(s, 0L, 2L, 9L); return (long) h.get(s, 0L, 2L) + " " + s.get(ValueLayout.JAVA_LONG, 40) + " " + h.coordinateTypes(); } });
        p("vh.arrayElement.struct.oob", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(pt, 3); return (long) pt.arrayElementVarHandle(MemoryLayout.PathElement.groupElement("z")).get(s, 0L, 3L); } });
        p("vh.arrayElement.seq", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(arr, 2); VarHandle h = arr.arrayElementVarHandle(MemoryLayout.PathElement.sequenceElement(), MemoryLayout.PathElement.groupElement("x")); h.set(s, 0L, 1L, 2L, 3); return (int) h.get(s, 0L, 1L, 2L) + " " + h.coordinateTypes(); } });
        p("vh.arrayElement.neg", () -> { try (Arena x = Arena.ofConfined()) { return (int) ValueLayout.JAVA_INT.arrayElementVarHandle().get(x.allocate(16), 0L, -1L); } });
        p("vh.arrayElement.closed", () -> { Arena x = Arena.ofConfined(); MemorySegment s = x.allocate(16); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); x.close(); return (int) h.get(s, 0L, 1L); });
        p("vh.arrayElement.thread", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); return onOther(() -> (int) h.get(s, 0L, 1L)); } });
        p("vh.arrayElement.heap", () -> { int[] b = new int[4]; VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); h.set(MemorySegment.ofArray(b), 0L, 2L, 5); return b[2]; });
        p("vh.arrayElement.addr", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16); VarHandle h = ValueLayout.ADDRESS.arrayElementVarHandle(); h.set(s, 0L, 1L, MemorySegment.ofAddress(9)); return ((MemorySegment) h.get(s, 0L, 1L)).address(); } });
        p("vh.arrayElement.path.seq", () -> MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_INT).arrayElementVarHandle(MemoryLayout.PathElement.sequenceElement()).coordinateTypes());
        p("vh.arrayElement.zeroSize", () -> MemoryLayout.paddingLayout(0).arrayElementVarHandle());
        p("vh.arrayElement.off", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(32); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); h.set(s, 8L, 2L, 5); return s.get(ValueLayout.JAVA_INT, 16); } });
        p("vh.withInvokeExactBehavior", () -> ValueLayout.JAVA_INT.varHandle().withInvokeExactBehavior().hasInvokeExactBehavior());
        p("vh.accessModes", () -> ValueLayout.JAVA_INT.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD) + " " + ValueLayout.JAVA_FLOAT.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD) + " " + ValueLayout.JAVA_FLOAT.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_OR) + " " + ValueLayout.ADDRESS.varHandle().isAccessModeSupported(VarHandle.AccessMode.COMPARE_AND_SET) + " " + ValueLayout.JAVA_LONG.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET_AND_BITWISE_XOR));
        p("vh.accessModes.unaligned", () -> ValueLayout.JAVA_INT_UNALIGNED.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD) + " " + ValueLayout.JAVA_INT_UNALIGNED.varHandle().isAccessModeSupported(VarHandle.AccessMode.GET) + " " + ValueLayout.JAVA_INT.withByteAlignment(2).varHandle().isAccessModeSupported(VarHandle.AccessMode.COMPARE_AND_SET));
        p("vh.cas.unaligned", () -> { try (Arena x = Arena.ofConfined()) { return ValueLayout.JAVA_INT_UNALIGNED.varHandle().compareAndSet(x.allocate(16, 8), 0L, 0, 1); } });
        p("vh.toMethodHandle", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); MethodHandle mh = ValueLayout.JAVA_INT.varHandle().toMethodHandle(VarHandle.AccessMode.SET); mh.invoke(s, 0L, 9); return s.get(ValueLayout.JAVA_INT, 0) + " " + mh.type(); } });
        p("vh.accessModeType", () -> ValueLayout.JAVA_INT.varHandle().accessModeType(VarHandle.AccessMode.GET) + " " + ValueLayout.JAVA_INT.varHandle().accessModeType(VarHandle.AccessMode.COMPARE_AND_SET));
        p("vh.arrayElement.accessModeType", () -> ValueLayout.JAVA_INT.arrayElementVarHandle().accessModeType(VarHandle.AccessMode.GET) + " " + ValueLayout.JAVA_INT.arrayElementVarHandle().accessModeType(VarHandle.AccessMode.SET));
        p("vh.arrayElement.toString.hasType", () -> ValueLayout.JAVA_INT.arrayElementVarHandle().varType() + " " + ValueLayout.JAVA_INT.arrayElementVarHandle().coordinateTypes());
        p("vh.insertCoordinates", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 4, 12); VarHandle h = MethodHandles.insertCoordinates(ValueLayout.JAVA_INT.varHandle(), 1, 4L); return (int) h.get(s) + " " + h.coordinateTypes(); } });
        p("vh.loop", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(4 * 1000); VarHandle h = ValueLayout.JAVA_INT.arrayElementVarHandle(); for (int i = 0; i < 1000; i++) h.set(s, 0L, (long) i, i * 2); long t = 0; for (int i = 0; i < 1000; i++) t += (int) h.get(s, 0L, (long) i); return t; } });
        p("vh.loop.plain", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(4 * 1000); VarHandle h = ValueLayout.JAVA_INT.varHandle(); for (int i = 0; i < 1000; i++) h.set(s, (long) i * 4, i * 2); long t = 0; for (int i = 0; i < 1000; i++) t += (int) h.get(s, (long) i * 4); return t; } });
    }

    static void fd() {
        p("fd.of", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        p("fd.ofVoid", () -> FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        p("fd.ofVoid.empty", () -> FunctionDescriptor.ofVoid());
        p("fd.of.noargs", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT));
        p("fd.of.nullres", () -> FunctionDescriptor.of(null, ValueLayout.JAVA_INT));
        p("fd.of.nullarg", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, (MemoryLayout) null));
        p("fd.of.nullarr", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, (MemoryLayout[]) null));
        p("fd.ofVoid.nullarr", () -> FunctionDescriptor.ofVoid((MemoryLayout[]) null));
        p("fd.of.padding", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, MemoryLayout.paddingLayout(4)));
        p("fd.ofVoid.padding", () -> FunctionDescriptor.ofVoid(MemoryLayout.paddingLayout(4)));
        p("fd.of.padres", () -> FunctionDescriptor.of(MemoryLayout.paddingLayout(4)));
        p("fd.accessors", () -> { FunctionDescriptor d = FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS); return d.returnLayout() + " " + d.argumentLayouts() + " " + d.toMethodType(); });
        p("fd.void.accessors", () -> { FunctionDescriptor d = FunctionDescriptor.ofVoid(ValueLayout.JAVA_LONG); return d.returnLayout() + " " + d.argumentLayouts() + " " + d.toMethodType(); });
        p("fd.struct.mt", () -> FunctionDescriptor.of(MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), ValueLayout.JAVA_INT).toMethodType());
        p("fd.equals", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).equals(FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG)) + " " + FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).equals(FunctionDescriptor.of(ValueLayout.JAVA_INT)) + " " + (FunctionDescriptor.of(ValueLayout.JAVA_INT).hashCode() == FunctionDescriptor.of(ValueLayout.JAVA_INT).hashCode()));
        p("fd.appendArg", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT).appendArgumentLayouts(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
        p("fd.insertArg", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).insertArgumentLayouts(0, ValueLayout.JAVA_INT));
        p("fd.insertArg.bad", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).insertArgumentLayouts(3, ValueLayout.JAVA_INT));
        p("fd.changeRet", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT).changeReturnLayout(ValueLayout.JAVA_LONG));
        p("fd.dropRet", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT).dropReturnLayout());
        p("fd.argLayouts.immutable", () -> { FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).argumentLayouts().add(ValueLayout.JAVA_INT); return "no"; });
        p("fd.class.iface", () -> FunctionDescriptor.of(ValueLayout.JAVA_INT).getClass().isInterface());
        p("linker.native", () -> Linker.nativeLinker() == Linker.nativeLinker());
        p("linker.native.iface", () -> Linker.nativeLinker().getClass().isInterface());
        p("linker.defaultLookup", () -> Linker.nativeLinker().defaultLookup() != null);
        p("linker.variadic.opt", () -> Linker.Option.firstVariadicArg(1));
        p("linker.downcall.null", () -> Linker.nativeLinker().downcallHandle(null, FunctionDescriptor.ofVoid()));
        p("linker.downcall.nulladdr", () -> Linker.nativeLinker().downcallHandle(MemorySegment.NULL, FunctionDescriptor.ofVoid()));
        p("linker.downcall.type", () -> { MethodHandle h = Linker.nativeLinker().downcallHandle(FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG)); return h.type(); });
        p("linker.downcall.type.struct", () -> Linker.nativeLinker().downcallHandle(FunctionDescriptor.of(MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), ValueLayout.JAVA_LONG)).type());
        p("linker.downcall.type.addr", () -> Linker.nativeLinker().downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS)).type());
        p("linker.downcall.badlayout", () -> Linker.nativeLinker().downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT.withByteAlignment(1))));
        p("linker.downcall.seq", () -> Linker.nativeLinker().downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_INT))));
        p("linker.downcall.variadic.bad", () -> Linker.nativeLinker().downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), Linker.Option.firstVariadicArg(5)));
        p("linker.upcall.type", () -> { try (Arena x = Arena.ofConfined()) { MethodHandle mh = MethodHandles.lookup().findStatic(Sbr2W8FfmBase.class, "cmp", java.lang.invoke.MethodType.methodType(int.class, int.class, int.class)); MemorySegment up = Linker.nativeLinker().upcallStub(mh, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), x); return up.byteSize() + " " + up.isNative(); } });
        p("linker.upcall.badtype", () -> { try (Arena x = Arena.ofConfined()) { MethodHandle mh = MethodHandles.lookup().findStatic(Sbr2W8FfmBase.class, "cmp", java.lang.invoke.MethodType.methodType(int.class, int.class, int.class)); return Linker.nativeLinker().upcallStub(mh, FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), x).byteSize(); } });
        p("lookup.loader", () -> SymbolLookup.loaderLookup().find("no_such_symbol_zz").isPresent());
        p("lookup.default.missing", () -> Linker.nativeLinker().defaultLookup().find("no_such_symbol_zz").isPresent());
        p("lookup.libraryLookup.missing", () -> { try (Arena x = Arena.ofConfined()) { return SymbolLookup.libraryLookup("no_such_lib_zz.so", x); } });
        p("lookup.or", () -> SymbolLookup.loaderLookup().or(SymbolLookup.loaderLookup()).find("zzz").isPresent());
    }

    static int cmp(int a, int b) { return a - b; }

    static void misc() {
        p("sal.slicing", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = SegmentAllocator.slicingAllocator(x.allocate(64)); MemorySegment a1 = al.allocate(8), a2 = al.allocate(8); return (a2.address() - a1.address()) + " " + a1.byteSize(); } });
        p("sal.slicing.exhaust", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = SegmentAllocator.slicingAllocator(x.allocate(16)); al.allocate(8); al.allocate(8); return al.allocate(1); } });
        p("sal.prefix", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = SegmentAllocator.prefixAllocator(x.allocate(16)); MemorySegment a1 = al.allocate(8); MemorySegment a2 = al.allocate(8); return a1.address() == a2.address(); } });
        p("sal.prefix.exhaust", () -> { try (Arena x = Arena.ofConfined()) { return SegmentAllocator.prefixAllocator(x.allocate(4)).allocate(8); } });
        p("sal.lambda", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); return al.allocate(ValueLayout.JAVA_INT).byteSize() + " " + al.allocateFrom("hi").byteSize() + " " + al.allocateFrom(ValueLayout.JAVA_INT, 1, 2).byteSize() + " " + al.allocate(ValueLayout.JAVA_INT, 3).byteSize(); } });
        p("sal.lambda.alloc", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); return al.allocate(12).byteSize(); } });
        p("sal.lambda.from", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 0, 4); return al.allocateFrom(ValueLayout.JAVA_INT, s, ValueLayout.JAVA_INT, 0, 2).get(ValueLayout.JAVA_INT, 0); } });
        p("sal.lambda.fromseg.bad", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); return al.allocateFrom(ValueLayout.JAVA_INT, x.allocate(8), ValueLayout.JAVA_INT, 0, 3); } });
        p("sal.lambda.neg", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); return al.allocate(-1); } });
        p("sal.lambda.badalign", () -> { try (Arena x = Arena.ofConfined()) { SegmentAllocator al = (sz, al2) -> x.allocate(sz, al2); return al.allocate(4, 5); } });
        p("sal.arena.is", () -> { try (Arena x = Arena.ofConfined()) { return (x instanceof SegmentAllocator) + " " + (x.allocate(4).scope() instanceof MemorySegment.Scope); } });
        p("sal.arena.classiface", () -> { try (Arena x = Arena.ofConfined()) { return x.getClass().isInterface() + " " + Arena.ofAuto().getClass().isInterface() + " " + Arena.global().getClass().isInterface() + " " + Arena.ofShared().getClass().isInterface(); } });
        p("sal.arena.custom", () -> { Arena a = new Arena() {
            Arena d = Arena.ofConfined();
            public MemorySegment allocate(long b, long al) { return d.allocate(b, al); }
            public MemorySegment.Scope scope() { return d.scope(); }
            public void close() { d.close(); } };
            try (Arena x = a) { return x.allocate(8).byteSize() + " " + x.allocateFrom("z").byteSize(); } });
        p("scope.iface", () -> { try (Arena x = Arena.ofConfined()) { return x.scope().getClass().isInterface() + " " + (x.scope() == x.scope()) + " " + x.scope().equals(x.scope()) + " " + (x.scope().hashCode() == x.scope().hashCode()); } });
        p("scope.global.eq", () -> Arena.global().scope().equals(MemorySegment.NULL.scope()) + " " + (Arena.global().scope() == Arena.global().scope()));
        p("scope.auto.eq", () -> { Arena a = Arena.ofAuto(); return a.scope().equals(a.allocate(1).scope()); });
        p("scope.auto.vs.global", () -> Arena.ofAuto().scope().equals(Arena.global().scope()));
        p("scope.heap", () -> MemorySegment.ofArray(new byte[1]).scope().isAlive() + " " + MemorySegment.ofArray(new byte[1]).scope().equals(MemorySegment.ofArray(new byte[1]).scope()) + " " + (MemorySegment.ofArray(new byte[1]).scope() == Arena.global().scope()));
        p("scope.buffer", () -> MemorySegment.ofBuffer(java.nio.ByteBuffer.allocateDirect(4)).scope().isAlive());
        p("scope.bb.arena", () -> { Arena x = Arena.ofConfined(); var bb = x.allocate(8).asByteBuffer(); boolean a = bb.isDirect(); x.close(); return a + " " + MemorySegment.ofBuffer(java.nio.ByteBuffer.allocateDirect(4)).byteSize(); });
        p("scope.ofBuffer.closed", () -> { Arena x = Arena.ofConfined(); var bb = x.allocate(8).asByteBuffer(); MemorySegment s = MemorySegment.ofBuffer(bb); x.close(); return s.get(ValueLayout.JAVA_BYTE, 0); });
        p("scope.ofBuffer.thread", () -> { try (Arena x = Arena.ofConfined()) { var bb = x.allocate(8).asByteBuffer(); MemorySegment s = MemorySegment.ofBuffer(bb); return onOther(() -> s.get(ValueLayout.JAVA_BYTE, 0)); } });
        p("scope.bb.heap.ofBuffer", () -> { var bb = java.nio.ByteBuffer.wrap(new byte[8]); return MemorySegment.ofBuffer(bb).scope().isAlive(); });
        p("scope.bb.slice", () -> { var bb = java.nio.ByteBuffer.allocateDirect(16); bb.position(4); bb.limit(12); MemorySegment s = MemorySegment.ofBuffer(bb); return s.byteSize(); });
        p("scope.bb.readonly", () -> { var bb = java.nio.ByteBuffer.allocateDirect(16).asReadOnlyBuffer(); MemorySegment s = MemorySegment.ofBuffer(bb); return s.isReadOnly(); });
        p("misc.getbuf.int", () -> { try (Arena x = Arena.ofConfined()) { var ib = x.allocate(16).asByteBuffer().asIntBuffer(); ib.put(1, 7); return ib.get(1) + " " + ib.capacity(); } });
        p("misc.bb.order", () -> { try (Arena x = Arena.ofConfined()) { var bb = x.allocate(16).asByteBuffer(); return bb.order() + " " + bb.putInt(0, 1).getInt(0); } });
        p("misc.bb.heap.arena", () -> { var bb = java.nio.ByteBuffer.wrap(new byte[8]); return MemorySegment.ofBuffer(bb).get(ValueLayout.JAVA_BYTE, 0); });
        p("misc.threadlocal.alloc", () -> { long t = 0; for (int i = 0; i < 100; i++) { try (Arena x = Arena.ofConfined()) { t += x.allocate(64).byteSize(); } } return t; });
        p("misc.shared.alloc", () -> { long t = 0; for (int i = 0; i < 100; i++) { try (Arena x = Arena.ofShared()) { t += x.allocate(64).byteSize(); } } return t; });
        p("misc.auto.alloc", () -> { long t = 0; for (int i = 0; i < 100; i++) { t += Arena.ofAuto().allocate(64).byteSize(); } return t; });
        p("misc.pointer.chain", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment a = x.allocate(8), b = x.allocate(8); b.set(ValueLayout.JAVA_LONG, 0, 99L); a.set(ValueLayout.ADDRESS, 0, b); return a.get(ValueLayout.ADDRESS, 0).reinterpret(8).get(ValueLayout.JAVA_LONG, 0); } });
        p("misc.pointer.eq", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment a = x.allocate(8), b = x.allocate(8); b.set(ValueLayout.ADDRESS, 0, a); return b.get(ValueLayout.ADDRESS, 0).address() == a.address(); } });
        p("misc.pointer.size0", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment a = x.allocate(8), b = x.allocate(8); b.set(ValueLayout.ADDRESS, 0, a); return b.get(ValueLayout.ADDRESS, 0).byteSize(); } });
        p("misc.pointer.get0", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment a = x.allocate(8), b = x.allocate(8); b.set(ValueLayout.ADDRESS, 0, a); return b.get(ValueLayout.ADDRESS, 0).get(ValueLayout.JAVA_BYTE, 0); } });
        p("misc.set.address.heap", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).set(ValueLayout.ADDRESS, 0, MemorySegment.ofArray(new byte[1])); return "no"; } });
        p("misc.set.address.null", () -> { try (Arena x = Arena.ofConfined()) { x.allocate(8).set(ValueLayout.ADDRESS, 0, (MemorySegment) null); return "no"; } });
        p("misc.linkermixed", () -> "done");
    }
}
