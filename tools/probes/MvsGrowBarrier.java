import java.nio.ByteBuffer;

/**
 * The shape of {@code org.h2.mvstore.WriteBuffer.ensureCapacity}/{@code grow},
 * re-implemented with no H2 on the classpath: a FIELD holds the buffer, a call
 * that {@code ensureCapacity} makes reassigns it, and {@code ensureCapacity}
 * re-reads the field on the return path.
 *
 * <pre>
 * ByteBuffer ensureCapacity(int len) {
 *     if (buff.remaining() &lt; len) { grow(len); }   // grow() assigns this.buff
 *     return buff;                                  // must be the NEW one
 * }
 * </pre>
 *
 * <h2>What this refutes</h2>
 *
 * A JIT that cached {@code this.buff} across the {@code grow()} call would hand
 * the caller the OLD, too-small buffer, and the very next write would raise
 * {@code BufferOverflowException} — which is the only mechanism that fits
 * H2's arithmetic, since the buffer is sized {@code 3 * len} and at most three
 * bytes are written per char. So this probe asserts, on every round, that the
 * buffer {@code ensureCapacity} returned really does have {@code 3 * len}
 * remaining, and forces {@code grow()} to run again every 97 rounds so the
 * reassignment happens thousands of times inside a hot loop rather than
 * settling after the first few.
 *
 * <p>Clean at 4 124 forced grows on G1, ZGC and Generational, JIT and
 * {@code --nojit}, at {@code -Xmx256m} and {@code -Xmx64m}. See
 * {@code probes/MvsWriteBuffer.java} for the same question asked of the real
 * class.
 */
public class MvsGrowBarrier {
    static final int MIN_GROW = 1024 * 1024;

    ByteBuffer buff = ByteBuffer.allocate(16);

    ByteBuffer ensureCapacity(int len) {
        if (buff.remaining() < len) {
            grow(len);
        }
        return buff;
    }

    void grow(int additional) {
        ByteBuffer temp = buff;
        int needed = additional - temp.remaining();
        long g = Math.max(needed, MIN_GROW);
        g = Math.max(temp.capacity() / 2, g);
        int newCapacity = (int) Math.min(Integer.MAX_VALUE, temp.capacity() + g);
        if (newCapacity < needed) {
            throw new OutOfMemoryError("Capacity: " + newCapacity);
        }
        buff = ByteBuffer.allocate(newCapacity);
        temp.flip();
        buff.put(temp);
    }

    void putStringData(String s, int len) {
        ByteBuffer b = ensureCapacity(3 * len);
        if (b.remaining() < 3 * len) {
            throw new IllegalStateException("STALE BUFFER: remaining=" + b.remaining()
                    + " need=" + (3 * len) + " cap=" + b.capacity()
                    + " fieldCap=" + buff.capacity() + " same=" + (b == buff));
        }
        for (int i = 0; i < len; i++) {
            int c = s.charAt(i);
            if (c < 0x80) {
                b.put((byte) c);
            } else if (c >= 0x800) {
                b.put((byte) (0xe0 | (c >> 12)));
                b.put((byte) ((c >> 6) & 0x3f));
                b.put((byte) (c & 0x3f));
            } else {
                b.put((byte) (0xc0 | (c >> 6)));
                b.put((byte) (c & 0x3f));
            }
        }
    }

    public static void main(String[] a) {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 400000;
        MvsGrowBarrier p = new MvsGrowBarrier();
        String big = new String(new char[3000]).replace("\0", "c");
        int resets = 0;
        for (int i = 0; i < rounds; i++) {
            String s = "Hello World " + i * 10;
            p.putStringData(s, s.length());
            if ((i % 97) == 0) {
                // Force grow() to run again on the next round.
                p.buff = ByteBuffer.allocate(16);
                resets++;
            }
            if ((i % 1013) == 0) {
                p.putStringData(big, big.length());
            }
        }
        System.out.println("rounds=" + rounds + " resets=" + resets
                + " finalCap=" + p.buff.capacity());
        System.out.println("DONE");
    }
}
