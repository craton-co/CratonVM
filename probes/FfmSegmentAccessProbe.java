import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * Every `MemorySegment.get`/`set` carrier, one section each.
 *
 * `java.lang.foreign.MemorySegment` declares nine `get`/`set` pairs and all
 * eighteen are `public abstract`, so each one needs an implementation of its
 * own. CratonVM registers them in two files and they disagree about which:
 * `panama.rs` enumerates all nine `get` descriptors and only the erased `set`,
 * `foreign_ffm.rs` covers Byte/Short/Int/Long for both. A carrier nobody
 * registers reaches the interface method itself and raises
 * `AbstractMethodError: … has no Code attribute` — which is how
 * `set(JAVA_DOUBLE, …)` was found.
 *
 * Each section is wrapped so one missing carrier cannot truncate the run and
 * hide the eight behind it; that is the whole reason this probe exists rather
 * than a single straight-line method.
 *
 * The values are chosen to catch the conversions, not just the plumbing:
 *
 *   * `char` above 0x7FFF — a sign-extended read answers negative;
 *   * `boolean` written as a raw byte 2 through a byte view — the JDK answers
 *     `true`, a raw pass-through answers 2;
 *   * negative and fractional floats/doubles — a bit-pattern that survives an
 *     integer path by accident would not survive these;
 *   * an address round-trip — the descriptor returns a `MemorySegment`, not a
 *     long, and that is a different Value kind entirely.
 */
public class FfmSegmentAccessProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) {
        section("byte", FfmSegmentAccessProbe::carrierByte);
        section("boolean", FfmSegmentAccessProbe::carrierBoolean);
        section("char", FfmSegmentAccessProbe::carrierChar);
        section("short", FfmSegmentAccessProbe::carrierShort);
        section("int", FfmSegmentAccessProbe::carrierInt);
        section("long", FfmSegmentAccessProbe::carrierLong);
        section("float", FfmSegmentAccessProbe::carrierFloat);
        section("double", FfmSegmentAccessProbe::carrierDouble);
        section("address", FfmSegmentAccessProbe::carrierAddress);
        section("atIndex", FfmSegmentAccessProbe::atIndex);
        System.out.println("FFMSEG sections=" + sections + " failed=" + failed);
    }

    static void carrierByte() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_BYTE, 0, (byte) 7);
            s.set(ValueLayout.JAVA_BYTE, 1, (byte) -128);
            System.out.println("byte a=" + s.get(ValueLayout.JAVA_BYTE, 0)
                    + " b=" + s.get(ValueLayout.JAVA_BYTE, 1));
        }
    }

    static void carrierBoolean() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_BOOLEAN, 0, true);
            s.set(ValueLayout.JAVA_BOOLEAN, 1, false);
            // A raw non-0/1 byte read back as a boolean: the JDK answers true.
            s.set(ValueLayout.JAVA_BYTE, 2, (byte) 2);
            System.out.println("boolean t=" + s.get(ValueLayout.JAVA_BOOLEAN, 0)
                    + " f=" + s.get(ValueLayout.JAVA_BOOLEAN, 1)
                    + " raw2=" + s.get(ValueLayout.JAVA_BOOLEAN, 2)
                    + " asByte=" + s.get(ValueLayout.JAVA_BYTE, 0));
        }
    }

    static void carrierChar() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_CHAR, 0, 'A');
            s.set(ValueLayout.JAVA_CHAR, 2, '￾');
            char high = s.get(ValueLayout.JAVA_CHAR, 2);
            System.out.println("char a=" + s.get(ValueLayout.JAVA_CHAR, 0)
                    + " highAsInt=" + ((int) high)
                    + " positive=" + (high > 0));
        }
    }

    static void carrierShort() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_SHORT, 0, (short) 4660);
            s.set(ValueLayout.JAVA_SHORT, 2, (short) -2);
            System.out.println("short a=" + s.get(ValueLayout.JAVA_SHORT, 0)
                    + " b=" + s.get(ValueLayout.JAVA_SHORT, 2));
        }
    }

    static void carrierInt() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_INT, 0, 0x0BADF00D);
            s.set(ValueLayout.JAVA_INT, 4, -1);
            System.out.println("int a=" + Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0))
                    + " b=" + s.get(ValueLayout.JAVA_INT, 4));
        }
    }

    static void carrierLong() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_LONG, 0, 0x0123456789ABCDEFL);
            s.set(ValueLayout.JAVA_LONG, 8, -1L);
            System.out.println("long a=" + Long.toHexString(s.get(ValueLayout.JAVA_LONG, 0))
                    + " b=" + s.get(ValueLayout.JAVA_LONG, 8));
        }
    }

    static void carrierFloat() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_FLOAT, 0, 1.5f);
            s.set(ValueLayout.JAVA_FLOAT, 4, -0.125f);
            // The bit pattern, so a value that survived an integer path by
            // accident is still visible as one.
            System.out.println("float a=" + s.get(ValueLayout.JAVA_FLOAT, 0)
                    + " b=" + s.get(ValueLayout.JAVA_FLOAT, 4)
                    + " bitsA=" + Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0)));
        }
    }

    static void carrierDouble() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_DOUBLE, 0, 1.5);
            s.set(ValueLayout.JAVA_DOUBLE, 8, -0.0625);
            System.out.println("double a=" + s.get(ValueLayout.JAVA_DOUBLE, 0)
                    + " b=" + s.get(ValueLayout.JAVA_DOUBLE, 8)
                    + " bitsA=" + Long.toHexString(s.get(ValueLayout.JAVA_LONG, 0)));
        }
    }

    static void carrierAddress() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment holder = a.allocate(64);
            MemorySegment target = a.allocate(16);
            holder.set(ValueLayout.ADDRESS, 0, target);
            MemorySegment back = holder.get(ValueLayout.ADDRESS, 0);
            System.out.println("address sameAddr=" + (back.address() == target.address())
                    + " nonZero=" + (back.address() != 0)
                    + " isSegment=" + (back instanceof MemorySegment));
        }
    }

    // getAtIndex/setAtIndex scale the offset by the layout size, so they are a
    // second way to reach every carrier and a check that the scaling agrees.
    static void atIndex() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.setAtIndex(ValueLayout.JAVA_INT, 3, 0xCAFE);
            s.setAtIndex(ValueLayout.JAVA_DOUBLE, 2, 2.25);
            System.out.println("atIndex int=" + Integer.toHexString(s.getAtIndex(ValueLayout.JAVA_INT, 3))
                    + " rawIntAt12=" + Integer.toHexString(s.get(ValueLayout.JAVA_INT, 12))
                    + " double=" + s.getAtIndex(ValueLayout.JAVA_DOUBLE, 2)
                    + " rawDoubleAt16=" + s.get(ValueLayout.JAVA_DOUBLE, 16));
        }
    }
}
