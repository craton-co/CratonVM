import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.reflect.Modifier;

/** Does a `MemorySegment` this VM mints report an INTERFACE as its class?
 *
 *  `HANDOFF-20260828-SCOPE.md` lists this as OPEN and unclaimed, "closest to
 *  L1". `panama.rs`'s own doc comment says the interface stamp was replaced on
 *  2026-08-22. One of the two is stale, and a comment is not a measurement.
 *
 *  What must hold, on every segment from every door: `getClass()` names a
 *  CONCRETE class. Real Java cannot have an instance whose class is an
 *  interface, and the JDK's own FFM consumers depend on it --
 *  `jdk.incubator.vector`'s `fromMemorySegment0Template` opens with
 *  `checkcast jdk/internal/foreign/AbstractMemorySegmentImpl`, which no
 *  interface stamp can satisfy.
 *
 *  The class NAME is a VM-implementation token and the two VMs may legally
 *  disagree on it, so the name is printed for the record but the assertions are
 *  on the properties a caller can rely on: not an interface, not abstract, and
 *  an `instanceof MemorySegment`.
 */
public class SegmentClassProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + v + "|");
    }

    static void describe(String tag, Object o) {
        if (o == null) { p(tag, "null"); return; }
        Class<?> c = o.getClass();
        p(tag + " isInterface", Modifier.isInterface(c.getModifiers()));
        p(tag + " isAbstract", Modifier.isAbstract(c.getModifiers()));
        p(tag + " instanceof MemorySegment", o instanceof MemorySegment);
        // The name itself is an implementation token: printed as a TAIL only
        // (the last path element), so the record has it without the diff
        // turning a legal disagreement into a failure.
        String n = c.getName();
        p(tag + " name looks concrete", !n.equals("java.lang.foreign.MemorySegment"));
    }

    static void t(String tag, Runnable r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    public static void main(String[] args) {
        // 1. a heap-backed segment
        describe("ofArray(byte[])", MemorySegment.ofArray(new byte[16]));
        describe("ofArray(int[])", MemorySegment.ofArray(new int[4]));
        describe("ofArray(long[])", MemorySegment.ofArray(new long[2]));

        // 2. an arena-allocated (off-heap) segment -- and the ARENA itself.
        // All four factories are asked: the first version of this probe
        // described only the SEGMENT an arena allocated, so `ofConfined` and
        // `ofShared` were never looked at and the family read as two-of-four
        // broken when it is four-of-four.
        try (Arena a = Arena.ofConfined()) {
            describe("Arena.ofConfined()", a);
            MemorySegment s = a.allocate(64);
            describe("Arena.ofConfined().allocate", s);
            describe("that segment's slice", s.asSlice(8, 16));
            describe("that segment's reinterpret", s.reinterpret(32));
            p("byteSize", s.byteSize());
            p("slice byteSize", s.asSlice(8, 16).byteSize());
            // a round-trip through the segment, so the carrier is exercised
            s.set(java.lang.foreign.ValueLayout.JAVA_INT, 0, 0x0BADF00D);
            p("int round-trip", Integer.toHexString(
                s.get(java.lang.foreign.ValueLayout.JAVA_INT, 0)));
            s.set(java.lang.foreign.ValueLayout.JAVA_LONG, 8, 0x0102030405060708L);
            p("long round-trip", Long.toHexString(
                s.get(java.lang.foreign.ValueLayout.JAVA_LONG, 8)));
        }

        // 3. Arena itself
        try (Arena sh = Arena.ofShared()) {
            describe("Arena.ofShared()", sh);
        }
        Arena auto = Arena.ofAuto();
        describe("Arena.ofAuto()", auto);
        describe("Arena.global()", Arena.global());
        describe("Arena.global().allocate", Arena.global().allocate(8));

        // 4. NULL and the zero-length segment
        describe("MemorySegment.NULL", MemorySegment.NULL);
        p("NULL byteSize", MemorySegment.NULL.byteSize());
        p("NULL address", MemorySegment.NULL.address());

        // 5. the refusals a caller depends on
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(16);
            t("slice past the end", () -> s.asSlice(8, 32));
            t("negative slice offset", () -> s.asSlice(-1, 4));
            t("get past the end", () ->
                s.get(java.lang.foreign.ValueLayout.JAVA_LONG, 12));
        }
        Arena closed = Arena.ofConfined();
        MemorySegment dead = closed.allocate(8);
        closed.close();
        t("read a segment whose arena is closed", () ->
            dead.get(java.lang.foreign.ValueLayout.JAVA_INT, 0));
        t("close an arena twice", closed::close);

        System.out.println("DONE SegmentClassProbe rows " + rows);
    }
}
