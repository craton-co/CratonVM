import java.lang.foreign.AddressLayout;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * The behavioural half of wave-2 step 3's `ValueLayout` conversion.
 *
 * Preparation of `java.lang.foreign.ValueLayout` used to pre-seed its static
 * constants with objects CratonVM fabricated two instance slots on — on classes
 * that are INTERFACES in the real JDK and have none — and suppress the real
 * `<clinit>` so nothing could contradict it. Under `--jdk-only` that pairing is
 * gone and the real `<clinit>` runs, so this probe is what says whether the
 * constants it produces are the right ones.
 *
 * Every line is a paired property, so a transcript diffs byte-for-byte against
 * HotSpot. `byteSize` and `byteAlignment` are the two the preseed invented;
 * the accessors and a real read/write through a segment are what prove the
 * layout is usable and not merely well-printed.
 */
public class W2ValueLayoutProbe {
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
        section("constants", W2ValueLayoutProbe::constants);
        section("segment", W2ValueLayoutProbe::segment);
        System.out.println("VALUELAYOUT sections=" + sections + " failed=" + failed);
    }

    static void line(String name, ValueLayout l) {
        System.out.println("VL " + name
                + " size=" + l.byteSize()
                + " align=" + l.byteAlignment()
                + " order=" + l.order()
                + " carrier=" + l.carrier().getName());
    }

    static void constants() {
        // The byte order every layout inherits. `ValueLayout.<clinit>` builds
        // its constants from `ByteOrder.nativeOrder()`, so if this line is
        // wrong every line below it is wrong for the same reason.
        System.out.println("VL nativeOrder=" + java.nio.ByteOrder.nativeOrder()
                + " bigEndianProp=" + System.getProperty("sun.cpu.endian"));
        line("JAVA_BYTE", ValueLayout.JAVA_BYTE);
        line("JAVA_BOOLEAN", ValueLayout.JAVA_BOOLEAN);
        line("JAVA_CHAR", ValueLayout.JAVA_CHAR);
        line("JAVA_SHORT", ValueLayout.JAVA_SHORT);
        line("JAVA_INT", ValueLayout.JAVA_INT);
        line("JAVA_LONG", ValueLayout.JAVA_LONG);
        line("JAVA_FLOAT", ValueLayout.JAVA_FLOAT);
        line("JAVA_DOUBLE", ValueLayout.JAVA_DOUBLE);
        line("JAVA_INT_UNALIGNED", ValueLayout.JAVA_INT_UNALIGNED);
        line("JAVA_LONG_UNALIGNED", ValueLayout.JAVA_LONG_UNALIGNED);
        AddressLayout a = ValueLayout.ADDRESS;
        System.out.println("VL ADDRESS size=" + a.byteSize() + " align=" + a.byteAlignment()
                + " order=" + a.order() + " carrier=" + a.carrier().getName());
        AddressLayout au = ValueLayout.ADDRESS_UNALIGNED;
        System.out.println("VL ADDRESS_UNALIGNED size=" + au.byteSize()
                + " align=" + au.byteAlignment());
    }

    // A layout is only right if something can be stored through it and read
    // back; printing `byteSize` proves the object exists, not that it works.
    static void segment() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(64);
            seg.set(ValueLayout.JAVA_INT, 0, 0x0BADF00D);
            seg.set(ValueLayout.JAVA_LONG, 8, -1L);
            seg.set(ValueLayout.JAVA_DOUBLE, 16, 1.5);
            seg.set(ValueLayout.JAVA_BYTE, 24, (byte) 7);
            System.out.println("VL rw"
                    + " int=" + Integer.toHexString(seg.get(ValueLayout.JAVA_INT, 0))
                    + " long=" + seg.get(ValueLayout.JAVA_LONG, 8)
                    + " double=" + seg.get(ValueLayout.JAVA_DOUBLE, 16)
                    + " byte=" + seg.get(ValueLayout.JAVA_BYTE, 24)
                    + " size=" + seg.byteSize());
        }
    }
}
