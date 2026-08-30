import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;

/** Does every FFM object this VM hands out have a CONCRETE class?
 *
 *  The identity residual recorded in
 *  `arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md`
 *  is that `getClass()` on an `Arena` or a `MemorySegment` answers the
 *  INTERFACE — which is impossible in Java, and which no `checkcast` the JDK
 *  emits can be made to tolerate in general.
 *
 *  **The class NAME is not asked, and must not be.** CratonVM's carrier is
 *  `cratonvm.internal.foreign.MemorySegmentImpl` where HotSpot's is
 *  `jdk.internal.foreign.NativeMemorySegmentImpl`; both are correct, because
 *  the concrete implementation of a JDK interface is not part of its contract
 *  and no conforming program may depend on it. What IS comparable — and what
 *  the defect is about — is whether the answer is a class at all:
 *
 *      getClass().isInterface()   HotSpot false everywhere
 *
 *  So every row here is a boolean that HotSpot answers the same way for every
 *  receiver, which makes a difference in either CratonVM mode a defect rather
 *  than a naming difference.
 *
 *  The relationships are asked beside it, because a concrete carrier that the
 *  JDK's own casts reject would be a worse answer than the interface stamp:
 *  `instanceof`, `Class.isInstance` and `isAssignableFrom` for the interfaces
 *  the JDK casts to.
 */
public class FfmCarrierProbe {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    /** The whole contract, per receiver: a concrete class that satisfies every
     *  interface the JDK casts it to. */
    static void carrier(String tag, Object o, Class<?>... ifaces) {
        p(tag + " is non-null", () -> o != null);
        p(tag + " class is concrete", () -> !o.getClass().isInterface());
        p(tag + " class is not an array", () -> !o.getClass().isArray());
        p(tag + " class is stable", () -> o.getClass() == o.getClass());
        for (Class<?> i : ifaces) {
            p(tag + " instanceof " + i.getSimpleName(), () -> i.isInstance(o));
            p(tag + " isAssignableFrom " + i.getSimpleName(),
                () -> i.isAssignableFrom(o.getClass()));
        }
        // A class-keyed map round trip: the identity defect would show up here
        // as two receivers colliding on one key when they should not.
        p(tag + " class as a map key", () -> {
            java.util.Map<Class<?>, String> m = new java.util.HashMap<>();
            m.put(o.getClass(), "v");
            return m.get(o.getClass());
        });
    }

    static void arenas() {
        carrier("[Arena.global]", Arena.global(), Arena.class, SegmentAllocator.class);
        carrier("[Arena.ofAuto]", Arena.ofAuto(), Arena.class, SegmentAllocator.class);
        try (Arena a = Arena.ofConfined()) {
            carrier("[Arena.ofConfined]", a, Arena.class, SegmentAllocator.class);
        }
        try (Arena a = Arena.ofShared()) {
            carrier("[Arena.ofShared]", a, Arena.class, SegmentAllocator.class);
        }
        p("two arenas share a class", () -> Arena.ofAuto().getClass() == Arena.ofAuto().getClass());
        p("global is a singleton", () -> Arena.global() == Arena.global());
    }

    static void segments() {
        carrier("[ofArray byte]", MemorySegment.ofArray(new byte[8]), MemorySegment.class);
        carrier("[ofArray int]", MemorySegment.ofArray(new int[4]), MemorySegment.class);
        carrier("[ofArray long]", MemorySegment.ofArray(new long[2]), MemorySegment.class);
        carrier("[NULL]", MemorySegment.NULL, MemorySegment.class);
        try (Arena a = Arena.ofConfined()) {
            carrier("[arena.allocate]", a.allocate(16), MemorySegment.class);
            carrier("[allocateFrom]", a.allocateFrom(ValueLayout.JAVA_INT, 1, 2, 3),
                MemorySegment.class);
            MemorySegment s = a.allocate(32);
            carrier("[asSlice]", s.asSlice(8, 8), MemorySegment.class);
            carrier("[asReadOnly]", s.asReadOnly(), MemorySegment.class);
            carrier("[reinterpret]", s.reinterpret(16), MemorySegment.class);
        }
        p("two segments share a class", () -> {
            return MemorySegment.ofArray(new byte[1]).getClass()
                == MemorySegment.ofArray(new byte[2]).getClass();
        });
        // The values still have to be right — a concrete carrier that lost the
        // data would be a worse answer than an interface-stamped one.
        p("round trip through a slice", () -> {
            try (Arena a = Arena.ofConfined()) {
                MemorySegment s = a.allocate(16);
                s.set(ValueLayout.JAVA_INT, 0, 0x11223344);
                s.set(ValueLayout.JAVA_INT, 8, 0x55667788);
                return Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0)) + ","
                        + Integer.toHexString(s.asSlice(8, 8).get(ValueLayout.JAVA_INT, 0));
            }
        });
        p("byteSize after allocate", () -> {
            try (Arena a = Arena.ofConfined()) {
                return a.allocate(24).byteSize();
            }
        });
    }

    public static void main(String[] args) {
        sect("arenas", FfmCarrierProbe::arenas);
        sect("segments", FfmCarrierProbe::segments);
        System.out.println("rows " + rows);
        System.out.println("DONE FfmCarrierProbe");
    }
}
