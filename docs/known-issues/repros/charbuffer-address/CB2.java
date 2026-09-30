import java.lang.reflect.*;
import java.nio.*;
public class CB2 {
    public static void main(String[] a) throws Exception {
        Class<?> uc = Class.forName("jdk.internal.misc.Unsafe");
        Method gu = uc.getDeclaredMethod("getUnsafe"); gu.setAccessible(true);
        Object u = gu.invoke(null);
        Method abo = uc.getMethod("arrayBaseOffset", Class.class);
        Method ais = uc.getMethod("arrayIndexScale", Class.class);
        System.out.println("char[] base=" + abo.invoke(u, char[].class) + " scale=" + ais.invoke(u, char[].class));
        System.out.println("byte[] base=" + abo.invoke(u, byte[].class) + " scale=" + ais.invoke(u, byte[].class));
        // What address does a heap CharBuffer actually carry?
        CharBuffer cb = CharBuffer.allocate(16);
        Field addr = Buffer.class.getDeclaredField("address"); addr.setAccessible(true);
        System.out.println("HeapCharBuffer.address=" + addr.getLong(cb));
        // Direct Unsafe copy between two char[]s at the documented base offset.
        Method cm = uc.getMethod("copyMemory", Object.class, long.class, Object.class, long.class, long.class);
        char[] src = new char[8], dst = new char[8];
        for (int i = 0; i < 8; i++) src[i] = (char)('A'+i);
        long base = ((Number) abo.invoke(u, char[].class)).longValue();
        try {
            cm.invoke(u, src, base, dst, base, 16L);
            System.out.println("copyMemory char[] OK dst=" + new String(dst));
        } catch (Throwable e) { System.out.println("copyMemory char[] THREW " + e.getCause()); }
    }
}
