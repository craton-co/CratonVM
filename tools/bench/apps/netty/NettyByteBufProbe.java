import io.netty.buffer.ByteBuf;
import io.netty.buffer.PooledByteBufAllocator;
import io.netty.buffer.UnpooledByteBufAllocator;
import io.netty.util.internal.PlatformDependent;

/** Checks real Netty access paths, then reports cold and warm loop costs separately. */
public final class NettyByteBufProbe {
    private static volatile long sink;

    private static long access(ByteBuf buffer, int count) {
        long sum = 0;
        for (int i = 0; i < count; i++) {
            int index = (i & 7) * 8;
            long value = 0x0123456789abcdefL ^ i;
            buffer.setLong(index, value);
            sum += buffer.getLong(index);
        }
        return sum;
    }

    private static void check(ByteBuf buffer) {
        long value = 0x0123456789abcdefL;
        buffer.setLong(1, value);
        if (buffer.getLong(1) != value || buffer.getByte(1) != 1
                || buffer.getLongLE(1) != Long.reverseBytes(value)) {
            throw new AssertionError("unaligned/endian " + buffer.getClass());
        }
        buffer.setLongLE(3, value);
        if (buffer.getLongLE(3) != value || buffer.getByte(3) != (byte) 0xef) {
            throw new AssertionError("little endian " + buffer.getClass());
        }
        try {
            buffer.getLong(buffer.capacity() - 7);
            throw new AssertionError("missing bounds check");
        } catch (IndexOutOfBoundsException expected) {
            // The default Netty bounds policy must survive inlining.
        }
        ByteBuf slice = buffer.slice(8, 32);
        slice.setLong(1, value);
        if (buffer.getLong(9) != value) {
            throw new AssertionError("slice offset");
        }
    }

    public static void main(String[] args) {
        int count = args.length == 0 ? 1_000_000 : Integer.parseInt(args[0]);
        if (count <= 0) throw new IllegalArgumentException("positive count required");
        long expected = 0;
        for (int i = 0; i < count; i++) expected += 0x0123456789abcdefL ^ i;
        System.out.println("unsafe=" + PlatformDependent.hasUnsafe());
        int kind = args.length < 2 ? 0 : Integer.parseInt(args[1]);
        if (kind < 0 || kind > 3) throw new IllegalArgumentException("kind 0..3 required");
        ByteBuf buffer;
        switch (kind) {
            case 0: buffer = UnpooledByteBufAllocator.DEFAULT.heapBuffer(64, 64); break;
            case 1: buffer = UnpooledByteBufAllocator.DEFAULT.directBuffer(64, 64); break;
            case 2: buffer = PooledByteBufAllocator.DEFAULT.heapBuffer(64, 64); break;
            default: buffer = PooledByteBufAllocator.DEFAULT.directBuffer(64, 64); break;
        }
        try {
            check(buffer);
            for (int round = 0; round < 5; round++) {
                long start = System.nanoTime();
                long actual = access(buffer, count);
                long elapsed = System.nanoTime() - start;
                if (actual != expected) throw new AssertionError("checksum " + actual);
                sink = actual;
                System.out.println(buffer.getClass().getSimpleName() + " round=" + round
                        + " ns=" + elapsed + " checksum=" + actual);
            }
        } finally {
            if (!buffer.release()) throw new AssertionError("release");
        }
    }
}
