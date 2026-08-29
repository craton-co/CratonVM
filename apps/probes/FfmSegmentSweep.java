import java.lang.foreign.*;
import java.nio.ByteOrder;
import java.util.*;

/** The `java.lang.foreign` segment surface, and specifically the one Phase-1
 *  row `the-definition-of-done-screen-run-for-the-first-time-20260828` §4 left
 *  open:
 *
 *      `cratonvm/internal/foreign/MemorySegmentImpl`, requested at panama.rs:191.
 *      When `craton_segment_class_id` is refused, `alloc_segment_carrier`
 *      allocates an object whose class is `java.lang.foreign.MemorySegment` —
 *      the INTERFACE.
 *
 *  That record measured ONE row (`getClass().getName()`) and reasoned about the
 *  rest. An instance whose class is an interface is a thing the Java object
 *  model does not contain, so this probe asks what actually happens to the
 *  moves such an object breaks — `getSuperclass`, `isInstance`, `instanceof`,
 *  `getInterfaces`, `isInterface`, a class-keyed cache, `equals`/`hashCode` —
 *  alongside the ordinary segment contract.
 *
 *  DETERMINISM. No ADDRESS is ever printed: `MemorySegment.address()` is a real
 *  pointer and differs on every run and every VM. Only its RELATIONS are asked
 *  (non-zero, ordering within one segment, difference between two slices of one
 *  allocation). No arena identity hash, no timing, no size the platform chooses.
 *  Class NAMES are printed and are the point.
 */
public class FfmSegmentSweep {
    static int rows = 0;
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    /** The class of a segment, asked every way the object model allows. A class
     *  that is an INTERFACE fails `isInterface() == false` and has no
     *  superclass, which is how an impossible object announces itself. */
    static void classShape(String tag, Object o) {
        if (o == null) { p(tag + " class", "null"); return; }
        Class<?> c = o.getClass();
        p(tag + " class", c.getName());
        p(tag + " isInterface", c.isInterface());
        p(tag + " isArray", c.isArray());
        p(tag + " isPrimitive", c.isPrimitive());
        p(tag + " superclass", c.getSuperclass() == null ? "null" : c.getSuperclass().getName());
        p(tag + " isInstance of its own class", c.isInstance(o));
        p(tag + " instanceof MemorySegment", o instanceof MemorySegment);
        p(tag + " MemorySegment.isInstance", MemorySegment.class.isInstance(o));
        p(tag + " assignable to MemorySegment",
          MemorySegment.class.isAssignableFrom(c));
        // A class-keyed cache is the move the DoD record names as entitled to
        // assume this cannot happen.
        Map<Class<?>, String> byClass = new HashMap<>();
        byClass.put(c, "cached");
        p(tag + " class-keyed cache round-trips", byClass.get(o.getClass()));
        // Interfaces the class declares. A real impl class implements
        // MemorySegment; the interface itself does not implement itself.
        List<String> ifaces = new ArrayList<>();
        for (Class<?> i : c.getInterfaces()) ifaces.add(i.getName());
        Collections.sort(ifaces);
        p(tag + " getInterfaces", ifaces);
        p(tag + " getModifiers isAbstract",
          java.lang.reflect.Modifier.isAbstract(c.getModifiers()));
    }

    public static void main(String[] args) throws Exception {
        // ---- confined arena ------------------------------------------------
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(16);
            classShape("confined.allocate(16)", s);
            p("byteSize", s.byteSize());
            p("address is non-zero", s.address() != 0);
            p("isNative", s.isNative());
            p("isReadOnly", s.isReadOnly());
            p("scope is non-null", s.scope() != null);
            p("scope isAlive", s.scope().isAlive());

            // Values, which the record says already agree.
            s.set(ValueLayout.JAVA_INT, 0, 0x01020304);
            p("get int back", Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0)));
            s.set(ValueLayout.JAVA_LONG, 8, 0x0102030405060708L);
            p("get long back", Long.toHexString(s.get(ValueLayout.JAVA_LONG, 8)));
            p("toArray length", s.toArray(ValueLayout.JAVA_BYTE).length);

            // Slices: relations, never addresses.
            MemorySegment sl = s.asSlice(4, 8);
            classShape("asSlice(4,8)", sl);
            p("slice byteSize", sl.byteSize());
            p("slice address is base+4", sl.address() - s.address());
            p("slice of slice byteSize", sl.asSlice(2).byteSize());
            t("asSlice past the end", () -> s.asSlice(20, 4));
            t("asSlice negative offset", () -> s.asSlice(-1, 2));
            t("asSlice negative length", () -> s.asSlice(0, -1));

            MemorySegment ro = s.asReadOnly();
            classShape("asReadOnly", ro);
            p("read-only isReadOnly", ro.isReadOnly());
            t("write through a read-only segment",
              () -> ro.set(ValueLayout.JAVA_INT, 0, 1));
            p("read through a read-only segment",
              Integer.toHexString(ro.get(ValueLayout.JAVA_INT, 0)));

            // Bounds are the contract edge.
            t("get past the end", () -> s.get(ValueLayout.JAVA_INT, 16));
            t("get at a negative offset", () -> s.get(ValueLayout.JAVA_INT, -4));
            t("get straddling the end", () -> s.get(ValueLayout.JAVA_LONG, 12));
            t("set past the end", () -> s.set(ValueLayout.JAVA_BYTE, 16, (byte) 1));

            // equals/hashCode: two views of one allocation.
            MemorySegment again = s.asSlice(0, 16);
            p("slice(0,size) equals the whole", again.equals(s));
            p("segment equals itself", s.equals(s));
            p("hashCode is stable", s.hashCode() == s.hashCode());
            p("mismatch with itself", s.mismatch(s));
            p("mismatch with a differing slice", s.mismatch(sl) >= -1);

            // fill / copy
            s.fill((byte) 7);
            p("after fill", s.get(ValueLayout.JAVA_BYTE, 3));
            MemorySegment dst = a.allocate(16);
            MemorySegment.copy(s, 0, dst, 0, 16);
            p("after copy", dst.get(ValueLayout.JAVA_BYTE, 3));
            p("copy made them equal", s.mismatch(dst));
        }

        // ---- the closed arena: every access must refuse ---------------------
        Arena closed = Arena.ofConfined();
        MemorySegment dead = closed.allocate(8);
        closed.close();
        p("scope isAlive after close", dead.scope().isAlive());
        t("get after close", () -> dead.get(ValueLayout.JAVA_INT, 0));
        t("set after close", () -> dead.set(ValueLayout.JAVA_INT, 0, 1));
        t("byteSize after close", () -> dead.byteSize());
        t("close twice", () -> closed.close());
        t("allocate after close", () -> closed.allocate(8));

        // ---- the other arenas ----------------------------------------------
        classShape("global", Arena.global().allocate(8));
        try (Arena au = Arena.ofAuto()) { } catch (Throwable ignored) { }
        MemorySegment auto = Arena.ofAuto().allocate(8);
        classShape("ofAuto", auto);
        try (Arena shared = Arena.ofShared()) {
            classShape("ofShared", shared.allocate(8));
        }
        p("Arena.global class", Arena.global().getClass().getName());
        p("Arena.ofConfined class", arenaClass());
        p("global scope isAlive", Arena.global().scope().isAlive());
        t("Arena.global().close()", () -> Arena.global().close());

        // ---- NULL and zero-length ------------------------------------------
        classShape("MemorySegment.NULL", MemorySegment.NULL);
        p("NULL byteSize", MemorySegment.NULL.byteSize());
        p("NULL address", MemorySegment.NULL.address());
        try (Arena a = Arena.ofConfined()) {
            MemorySegment zero = a.allocate(0);
            p("zero-length byteSize", zero.byteSize());
            t("get from a zero-length segment", () -> zero.get(ValueLayout.JAVA_BYTE, 0));
            t("allocate(-1)", () -> a.allocate(-1));
            t("allocate(8, 0) bad alignment", () -> a.allocate(8, 0));
            t("allocate(8, 3) non-power-of-two", () -> a.allocate(8, 3));
        }

        // ---- heap segments --------------------------------------------------
        MemorySegment heap = MemorySegment.ofArray(new byte[16]);
        classShape("ofArray(byte[16])", heap);
        p("heap isNative", heap.isNative());
        p("heap byteSize", heap.byteSize());
        p("heap heapBase present", heap.heapBase().isPresent());
        // `JAVA_INT` carries a 4-byte alignment and a `byte[]` heap segment only
        // guarantees 1, so the ALIGNED accessor is a refusal on both VMs and the
        // UNALIGNED one is the write. Asking both is the contract; asking only
        // the first killed this probe at row 146 on HotSpot too.
        t("heap set aligned JAVA_INT", () -> heap.set(ValueLayout.JAVA_INT, 0, 1));
        heap.set(ValueLayout.JAVA_INT_UNALIGNED, 0, 0x0a0b0c0d);
        p("heap get back", Integer.toHexString(heap.get(ValueLayout.JAVA_INT_UNALIGNED, 0)));
        classShape("ofArray(int[4])", MemorySegment.ofArray(new int[4]));
        t("ofArray(null)", () -> MemorySegment.ofArray((byte[]) null));
        MemorySegment bb = MemorySegment.ofBuffer(java.nio.ByteBuffer.allocate(16));
        classShape("ofBuffer(heap)", bb);
        p("ofBuffer byteSize", bb.byteSize());

        // ---- layouts, which are the other half of the API -------------------
        p("JAVA_INT byteSize", ValueLayout.JAVA_INT.byteSize());
        p("JAVA_INT byteAlignment", ValueLayout.JAVA_INT.byteAlignment());
        p("JAVA_INT order is native",
          ValueLayout.JAVA_INT.order() == ByteOrder.nativeOrder());
        p("JAVA_INT class", ValueLayout.JAVA_INT.getClass().getName());
        p("JAVA_INT isInterface", ValueLayout.JAVA_INT.getClass().isInterface());
        StructLayout st = MemoryLayout.structLayout(
            ValueLayout.JAVA_INT.withName("a"),
            ValueLayout.JAVA_INT.withName("b"));
        p("struct byteSize", st.byteSize());
        p("struct class", st.getClass().getName());
        p("struct isInterface", st.getClass().isInterface());
        SequenceLayout sq = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_LONG);
        p("sequence byteSize", sq.byteSize());
        p("sequence elementCount", sq.elementCount());
        p("sequence class", sq.getClass().getName());
        try (Arena a = Arena.ofConfined()) {
            MemorySegment ss = a.allocate(st);
            p("allocate(struct) byteSize", ss.byteSize());
            classShape("allocate(struct)", ss);
        }

        System.out.println("rows " + rows + " DONE FfmSegmentSweep");
    }

    static String arenaClass() {
        try (Arena a = Arena.ofConfined()) { return a.getClass().getName(); }
    }
}
