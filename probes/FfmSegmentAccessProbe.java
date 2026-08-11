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
 * Run it with `--enable-native-access=ALL-UNNAMED`. CratonVM gates every
 * `MemorySegment.get` behind that flag as defence-in-depth (the accessor
 * dereferences the segment's raw pointer), and measured on 2026-08-10 the four
 * `set` carriers that had an implementation were the only ones NOT gated -- a
 * raw-address write guarded less than a read. Routing all nine through the same
 * implementation closes that too, so without the flag the probe now reports
 * `IllegalCallerException` uniformly instead of on the reads alone.
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
        // Which class is actually being dispatched on. A fabricated instance of
        // the INTERFACE finds only its abstract methods; a real
        // NativeMemorySegmentImpl finds bytecode. The two failure modes read
        // nothing alike and this one line tells them apart.
        //
        // It is the ONE line that cannot match HotSpot, and deliberately so:
        // CratonVM's Arena hands out an instance of java.lang.foreign.
        // MemorySegment itself where the JDK builds a
        // jdk.internal.foreign.NativeMemorySegmentImpl. That is a separate,
        // pre-existing CompatibilityClassRequested matter, and printing it
        // keeps a reader from mistaking it for one of the accessor defects --
        // every other line here IS expected to be byte-identical.
        section("shape", FfmSegmentAccessProbe::shape);
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
        section("getBoolChar", FfmSegmentAccessProbe::getBoolChar);
        section("getAddress", FfmSegmentAccessProbe::getAddress);
        System.out.println("FFMSEG sections=" + sections + " failed=" + failed);
    }

    static void shape() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            System.out.println("shape seg=" + s.getClass().getName()
                    + " arena=" + a.getClass().getName()
                    + " size=" + s.byteSize()
                    + " native=" + s.isNative());
        }
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

    // The three GET conversions, reached WITHOUT the set overloads that were
    // missing: write through a carrier that already works and read back
    // through the one under test. That is what makes these three lines
    // measurable on the pre-fix binary instead of only after the fix.
    static void getBoolChar() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_BYTE, 0, (byte) 2);    // -> read back as boolean
            s.set(ValueLayout.JAVA_SHORT, 2, (short) -2);  // -> read back as char (0xFFFE)
            char c = s.get(ValueLayout.JAVA_CHAR, 2);
            System.out.println("getBoolChar bool2=" + s.get(ValueLayout.JAVA_BOOLEAN, 0)
                    + " charAsInt=" + ((int) c)
                    + " charPositive=" + (c > 0));
        }
    }

    // Its own section: this one returns a REFERENCE, so when it is wrong it is
    // wrong by being null, and an NPE here would otherwise take the two
    // conversions above down with it.
    static void getAddress() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = a.allocate(64);
            s.set(ValueLayout.JAVA_LONG, 8, 0x1234L);
            MemorySegment addr = s.get(ValueLayout.ADDRESS, 8);
            System.out.println("getAddress null=" + (addr == null)
                    + " addr=" + (addr == null ? "-" : Long.toHexString(addr.address()))
                    + " isSegment=" + (addr instanceof MemorySegment));
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
