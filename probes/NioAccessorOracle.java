import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Differential oracle for `java.nio.ByteBuffer`'s absolute and relative
 * multi-byte accessors — the ones CratonVM services with the `s2_bb_read{2,4,8}`
 * / `s2_bb_write{2,4,8}` natives (`native-builtins/src/servlet.rs`).
 *
 * Those six used to resolve the buffer's backing storage once PER BYTE, which
 * made `putLong` cost 1088 ns against HotSpot's 0.30 (see `NioAccessorRate`).
 * Resolving once per access is a pure performance change, so the guard it needs
 * is a differential one: this prints a single checksum over every
 * width x endianness x storage-kind x alignment combination, plus the
 * out-of-range and read-only behaviour, and HotSpot and CratonVM must print the
 * SAME line.
 *
 *   java  NioAccessorOracle
 *   cratonvm --java-home <jdk> NioAccessorOracle
 */
public final class NioAccessorOracle {

    static long mix(long h, long v) { return h * 1000003L ^ v; }

    static long exercise(ByteBuffer b, String tag, StringBuilder log) {
        long h = 17;
        for (ByteOrder order : new ByteOrder[] { ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN }) {
            b.order(order);
            // Absolute accessors at every alignment inside the first 64 bytes.
            for (int idx = 0; idx + 8 <= 64; idx++) {
                b.putLong(idx, 0x0123456789ABCDEFL ^ (idx * 0x1111111111111111L));
                h = mix(h, b.getLong(idx));
                b.putInt(idx, 0x89ABCDEF ^ (idx * 0x11111111));
                h = mix(h, b.getInt(idx));
                b.putShort(idx, (short) (0xBEEF ^ (idx * 0x1111)));
                h = mix(h, b.getShort(idx));
                b.putChar(idx, (char) (0xCAFE ^ (idx * 0x1111)));
                h = mix(h, b.getChar(idx));
                b.putFloat(idx, Float.intBitsToFloat(0x40490FDB ^ idx));
                h = mix(h, Float.floatToRawIntBits(b.getFloat(idx)));
                b.putDouble(idx, Double.longBitsToDouble(0x400921FB54442D18L ^ idx));
                h = mix(h, Double.doubleToRawLongBits(b.getDouble(idx)));
                // Every individual byte the widest write left behind, so a
                // byte-order or offset slip cannot cancel out in the checksum.
                b.putLong(idx, 0x0011223344556677L);
                for (int k = 0; k < 8; k++) { h = mix(h, b.get(idx + k)); }
            }
            // Relative accessors, which go through position/limit as well.
            b.clear();
            for (int i = 0; i < 8; i++) { b.putLong(0x7766554433221100L + i); }
            b.flip();
            while (b.remaining() >= 8) { h = mix(h, b.getLong()); }
            b.clear();
            for (int i = 0; i < 16; i++) { b.putInt(0x11223344 + i); }
            b.flip();
            while (b.remaining() >= 4) { h = mix(h, b.getInt()); }
            b.clear();
        }
        log.append(tag).append('=').append(h).append('\n');
        return h;
    }

    static String oob(String what, Runnable r) {
        try { r.run(); return what + "=no-throw"; }
        catch (Throwable t) { return what + "=" + t.getClass().getName(); }
    }

    public static void main(String[] args) {
        StringBuilder log = new StringBuilder();
        long h = 17;

        h = mix(h, exercise(ByteBuffer.allocate(256), "heap", log));
        h = mix(h, exercise(ByteBuffer.allocateDirect(256), "direct", log));

        byte[] backing = new byte[300];
        h = mix(h, exercise(ByteBuffer.wrap(backing, 20, 256).slice(), "wrap-sliced", log));
        h = mix(h, exercise(ByteBuffer.allocate(300).position(13).limit(269).slice(), "heap-sliced", log));
        h = mix(h, exercise(ByteBuffer.allocateDirect(300).position(13).limit(269).slice(), "direct-sliced", log));
        h = mix(h, exercise(ByteBuffer.allocate(256).duplicate(), "heap-dup", log));

        // Out-of-range and read-only contracts must agree too: a "resolve once"
        // rewrite is exactly the kind of change that can turn a throw into a
        // silent zero.
        ByteBuffer small = ByteBuffer.wrap(new byte[8]);
        log.append(oob("getLong(1)-on-8", () -> small.getLong(1))).append('\n');
        log.append(oob("getLong(-1)", () -> small.getLong(-1))).append('\n');
        log.append(oob("getInt(6)-on-8", () -> small.getInt(6))).append('\n');
        log.append(oob("putLong(1)-on-8", () -> small.putLong(1, 1L))).append('\n');
        ByteBuffer ro = ByteBuffer.wrap(new byte[16]).asReadOnlyBuffer();
        log.append(oob("putLong-readonly", () -> ro.putLong(0, 1L))).append('\n');
        log.append(oob("putInt-readonly", () -> ro.putInt(0, 1))).append('\n');
        ByteBuffer lim = ByteBuffer.allocate(64);
        lim.limit(10);
        log.append(oob("getLong(4)-limit10", () -> lim.getLong(4))).append('\n');

        System.out.print(log);
        System.out.println("TOTAL=" + h);
    }
}
