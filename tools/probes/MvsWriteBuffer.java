import org.h2.mvstore.WriteBuffer;

/**
 * Drives H2's real {@code org.h2.mvstore.WriteBuffer} through exactly the
 * sequence {@code ObjectDataType.StringType.write} uses, and checks that it
 * never throws.
 *
 * <h2>What this refutes</h2>
 *
 * {@code TestMVStoreTool} was on record throwing
 * {@code BufferOverflowException} at {@code DataUtils.writeStringData} →
 * {@code HeapByteBuffer.put}, and the natural reading was that CratonVM
 * disagrees with the JDK about a {@code String}'s length or a
 * {@code ByteBuffer}'s remaining capacity. It cannot be that: every caller of
 * {@code writeStringData} in H2 reaches it through
 * {@code WriteBuffer.putStringData(s, len)}, which is
 * {@code DataUtils.writeStringData(ensureCapacity(3 * len), s, len)} — and
 * {@code writeStringData} writes at most three bytes per char over the same
 * {@code len}. So the buffer is sized from the same number the loop is bounded
 * by, and an overflow means {@code ensureCapacity} handed back a buffer with
 * less than {@code 3 * len} remaining. This probe checks exactly that, on the
 * real class, hot enough for the JIT to compile it.
 *
 * <p>Runs clean on G1, ZGC and Generational, JIT and {@code --nojit}. See
 * {@code probes/MvsGrowBarrier.java} for the field-reassignment half, and
 * {@code docs/internal/performance/h2-mvstoretool-create-phase-is-mutator-side-address-validation-20260907.md}.
 *
 * <pre>
 * cratonvm --java-home "$JDK" --Xmx 256m -c "/tmp/p:$H2CP" MvsWriteBuffer 2000000
 * </pre>
 */
public class MvsWriteBuffer {
    public static void main(String[] a) {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;

        WriteBuffer wb = new WriteBuffer();
        for (int i = 0; i < rounds; i++) {
            String s = "Hello World " + i * 10;
            int len = s.length();
            try {
                if (len <= 15) {
                    wb.put((byte) (48 + len));
                } else {
                    wb.put((byte) 2).putVarInt(len);
                }
                wb.putStringData(s, len);
            } catch (RuntimeException e) {
                System.out.println("THROW at round " + i + " len=" + len
                        + " cap=" + wb.capacity() + " pos=" + wb.position() + ": " + e);
                throw e;
            }
            if ((i & 0x3ff) == 0x3ff) {
                wb.clear();
            }
        }
        System.out.println("rounds=" + rounds);

        // Second shape: the 3000-char strings TestMVStoreTool's own
        // BIG_STRING_WITH_C/BIG_STRING_WITH_H fields carry, which force a grow
        // on a buffer that has just been cleared.
        WriteBuffer wb2 = new WriteBuffer();
        String big = new String(new char[3000]).replace("\0", "c");
        for (int i = 0; i < 20000; i++) {
            wb2.put((byte) 2).putVarInt(big.length());
            wb2.putStringData(big, big.length());
            if ((i & 0x1f) == 0x1f) {
                wb2.clear();
            }
        }
        System.out.println("bigrounds ok");
        System.out.println("DONE");
    }
}
