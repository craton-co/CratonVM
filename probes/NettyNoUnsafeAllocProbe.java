import io.netty.buffer.AdaptiveByteBufAllocator;
import io.netty.buffer.ByteBuf;
import io.netty.util.internal.PlatformDependent;

/**
 * The repro from docs/known-issues/netty/memorysegment-asbytebuffer-unimplemented.
 * With -Dio.netty.noUnsafe=true, netty takes HotSpot 25's own default path:
 * CleanerJava25 -> FFM Arena -> MemorySegment.asByteBuffer().
 */
public class NettyNoUnsafeAllocProbe {
    public static void main(String[] a) {
        System.out.println("hasUnsafe=" + PlatformDependent.hasUnsafe());
        try {
            Class<?> c = Class.forName("io.netty.util.internal.CleanerJava25");
            java.lang.reflect.Method m = c.getDeclaredMethod("isSupported");
            m.setAccessible(true);
            System.out.println("CleanerJava25.isSupported=" + m.invoke(null));
        } catch (Throwable t) {
            System.out.println("CleanerJava25 probe: " + t);
        }
        try {
            ByteBuf buf = new AdaptiveByteBufAllocator().directBuffer(256);
            System.out.println("cap=" + buf.capacity() + " direct=" + buf.isDirect());
            buf.writeInt(0x01020304);
            System.out.println("roundtrip=0x" + Integer.toHexString(buf.getInt(0)));
            buf.release();
            System.out.println("RESULT OK");
        } catch (Throwable t) {
            System.out.println("FAILED: " + t);
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) {
                System.out.println("  CAUSE: " + c);
            }
            System.out.println("RESULT FAIL");
        }
    }
}
