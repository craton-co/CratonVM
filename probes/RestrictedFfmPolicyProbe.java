import java.lang.foreign.*;

/** What does the JVM do when a restricted FFM method is called with NO
 *  --enable-native-access? JEP 472 made this a WARNING by default in JDK 24+,
 *  with --illegal-native-access=deny turning it into an exception. Infinispan's
 *  OffHeapMemoryAllocator calls reinterpret in a <clinit>, so a throw here
 *  becomes ExceptionInInitializerError and kills the cache manager. */
public class RestrictedFfmPolicyProbe {
    public static void main(String[] a) {
        System.out.println("  java.version = " + System.getProperty("java.version"));
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment s = arena.allocate(64);
            try {
                MemorySegment r = s.reinterpret(128);
                System.out.println("  reinterpret        -> OK, byteSize=" + r.byteSize());
            } catch (Throwable t) {
                System.out.println("  reinterpret        -> " + t.getClass().getName() + ": " + t.getMessage());
            }
            try {
                MemorySegment z = MemorySegment.ofAddress(0x1000L);
                System.out.println("  ofAddress          -> OK, byteSize=" + z.byteSize());
            } catch (Throwable t) {
                System.out.println("  ofAddress          -> " + t.getClass().getName() + ": " + t.getMessage());
            }
            try {
                s.set(ValueLayout.JAVA_BYTE, 0, (byte) 7);
                System.out.println("  set(JAVA_BYTE)     -> OK, got=" + s.get(ValueLayout.JAVA_BYTE, 0));
            } catch (Throwable t) {
                System.out.println("  set(JAVA_BYTE)     -> " + t.getClass().getName() + ": " + t.getMessage());
            }
        }
    }
}
