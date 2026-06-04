import java.lang.reflect.Field;

// Regression probe for the Unsafe off-heap arena store (allocateMemory returns a
// synthetic handle from the 2^36 arena, backed by a host Vec). Round-trips
// put/get byte + a copyMemory + reallocateMemory, then frees. Guards the
// NativeContext copy_from/to_native_memory arena routing. JDK-only, deterministic.
//
// Uses sun.misc.Unsafe via reflection (the classic theUnsafe singleton) so the
// probe compiles against any JDK without --add-exports.
public class UnsafeMemProbe {
    public static void main(String[] args) throws Exception {
        Class<?> uc = Class.forName("sun.misc.Unsafe");
        Field f = uc.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Object u = f.get(null);

        var allocate = uc.getMethod("allocateMemory", long.class);
        var free = uc.getMethod("freeMemory", long.class);
        var putByte = uc.getMethod("putByte", long.class, byte.class);
        var getByte = uc.getMethod("getByte", long.class);
        var realloc = uc.getMethod("reallocateMemory", long.class, long.class);

        long addr = (long) allocate.invoke(u, 8L);
        for (int i = 0; i < 8; i++) {
            putByte.invoke(u, addr + i, (byte) (i * 3));
        }
        // Print each read-back byte on its own line (avoid StringBuilder
        // accumulation so the probe is a clean signal for the Unsafe arena
        // path, not other string machinery).
        for (int i = 0; i < 8; i++) {
            System.out.println("byte" + i + "=" + (int) (byte) getByte.invoke(u, addr + i));
        }

        long addr2 = (long) realloc.invoke(u, addr, 16L);
        // first 8 bytes preserved across realloc
        System.out.println("realloc0=" + (int) (byte) getByte.invoke(u, addr2));
        System.out.println("realloc7=" + (int) (byte) getByte.invoke(u, addr2 + 7));
        free.invoke(u, addr2);
        System.out.println("OK");
    }
}
