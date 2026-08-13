import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/**
 * Audit of the java.lang.foreign surface CratonVM fabricates as INTERFACE
 * instances: every method with no registered native surfaces as
 * "AbstractMethodError: ... has no Code attribute" rather than as a missing
 * feature. One line per method, same order on both VMs, so a diff is the gap
 * list.
 */
public class FfmInterfaceAuditProbe {
    static void row(String name, Callable c) {
        String out;
        try {
            Object v = c.call();
            out = "OK   " + v;
        } catch (Throwable t) {
            String m = t.getMessage();
            if (m != null && m.length() > 90) m = m.substring(0, 90);
            out = "FAIL " + t.getClass().getName() + ": " + m;
        }
        System.out.println(String.format("%-28s %s", name, out));
    }

    interface Callable { Object call() throws Throwable; }

    public static void main(String[] args) throws Throwable {
        Arena ar = Arena.ofShared();
        MemorySegment seg = ar.allocate(64, 8);
        seg.set(ValueLayout.JAVA_INT, 0, 0x11223344);

        row("byteSize", () -> seg.byteSize());
        row("address", () -> Long.toHexString(seg.address()));
        row("isNative", () -> seg.isNative());
        row("isMapped", () -> seg.isMapped());
        row("isReadOnly", () -> seg.isReadOnly());
        row("scope!=null", () -> seg.scope() != null);
        row("maxByteAlignment", () -> seg.maxByteAlignment());
        row("heapBase", () -> seg.heapBase());
        row("isAccessibleBy", () -> seg.isAccessibleBy(Thread.currentThread()));
        row("asSlice(J)", () -> seg.asSlice(8).byteSize());
        row("asSlice(JJ)", () -> seg.asSlice(8, 16).byteSize());
        row("asSlice(JJJ)", () -> seg.asSlice(8, 16, 8).byteSize());
        row("asSlice(J,layout)", () -> seg.asSlice(8, ValueLayout.JAVA_INT).byteSize());
        row("asReadOnly.isReadOnly", () -> seg.asReadOnly().isReadOnly());
        row("reinterpret(J)", () -> seg.reinterpret(32).byteSize());
        row("fill", () -> seg.fill((byte) 7).byteSize());
        row("mismatch(self)", () -> seg.mismatch(seg));
        row("mismatch(other)", () -> {
            MemorySegment o = ar.allocate(64, 8);
            return seg.mismatch(o);
        });
        row("asOverlappingSlice", () -> seg.asOverlappingSlice(seg).isPresent());
        row("copyFrom", () -> {
            MemorySegment o = ar.allocate(64, 8);
            return o.copyFrom(seg).byteSize();
        });
        row("toArray(byte)", () -> seg.toArray(ValueLayout.JAVA_BYTE).length);
        row("toArray(int)", () -> seg.toArray(ValueLayout.JAVA_INT).length);
        row("elements(count)", () -> seg.elements(ValueLayout.JAVA_INT).count());
        row("spliterator", () -> seg.spliterator(ValueLayout.JAVA_INT).estimateSize());
        row("setString/getString", () -> {
            MemorySegment s = ar.allocate(32, 1);
            s.setString(0, "hello");
            return s.getString(0);
        });
        row("getString(charset)", () -> {
            MemorySegment s = ar.allocate(32, 1);
            s.setString(0, "hi", StandardCharsets.UTF_8);
            return s.getString(0, StandardCharsets.UTF_8);
        });
        row("equals/hashCode", () -> seg.equals(seg) && seg.hashCode() == seg.hashCode());
        row("isLoaded", () -> seg.isLoaded());

        // --- the headline: the FFM -> NIO bridge ---
        row("asByteBuffer", () -> {
            ByteBuffer bb = seg.asByteBuffer();
            return "cap=" + bb.capacity() + " direct=" + bb.isDirect()
                    + " ro=" + bb.isReadOnly() + " order=" + bb.order();
        });
        row("asByteBuffer.getInt", () -> {
            MemorySegment s = ar.allocate(8, 8);
            s.set(ValueLayout.JAVA_INT, 0, 0x0A0B0C0D);
            return "0x" + Integer.toHexString(s.asByteBuffer().getInt(0));
        });
        row("asByteBuffer.put->seg", () -> {
            MemorySegment s = ar.allocate(8, 8);
            ByteBuffer bb = s.asByteBuffer();
            bb.putInt(0, 0x01020304);
            return "0x" + Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0));
        });
        row("asByteBuffer(readOnly)", () -> {
            ByteBuffer bb = seg.asReadOnly().asByteBuffer();
            return "ro=" + bb.isReadOnly();
        });
        row("ofBuffer(direct)", () -> {
            ByteBuffer bb = ByteBuffer.allocateDirect(32);
            return MemorySegment.ofBuffer(bb).byteSize();
        });
        row("ofArray(byte[])", () -> MemorySegment.ofArray(new byte[16]).byteSize());
        row("ofArray(char[])", () -> MemorySegment.ofArray(new char[8]).byteSize());
        row("ofArray(short[])", () -> MemorySegment.ofArray(new short[8]).byteSize());

        // --- Arena / layout surface ---
        row("Arena.scope", () -> ar.scope() != null);
        row("Arena.allocateFrom(str)", () -> ar.allocateFrom("abc").byteSize());
        row("layout.byteSize", () -> ValueLayout.JAVA_INT.byteSize());
        row("MemoryLayout.seq", () -> MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteSize());
        row("MemoryLayout.struct", () -> MemoryLayout.structLayout(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteSize());

        ar.close();
        System.out.println("AUDIT-END");
    }
}
