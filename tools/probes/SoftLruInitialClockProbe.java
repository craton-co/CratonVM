import java.lang.ref.SoftReference;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/** The VM supplies a positive master clock before the first explicit GC.
 * Run with java.lang.ref and jdk.internal.misc opened to ALL-UNNAMED.
 */
public class SoftLruInitialClockProbe {
    public static void main(String[] args) throws Exception {
        Class<?> owner = Class.forName("java.lang.ref.SoftReference", false, null);
        Field clock = owner.getDeclaredField("clock");
        long raw = -1;
        Object unsafe = null;
        Object base = null;
        long offset = 0;
        Method read = null;
        if (args.length != 0 && args[0].equals("unsafe")) {
            Class<?> type = Class.forName("jdk.internal.misc.Unsafe");
            Field singleton = type.getDeclaredField("theUnsafe");
            singleton.setAccessible(true);
            unsafe = singleton.get(null);
            base = type.getMethod("staticFieldBase", Field.class).invoke(unsafe, clock);
            offset = (Long) type.getMethod("staticFieldOffset", Field.class).invoke(unsafe, clock);
            read = type.getMethod("getLong", Object.class, long.class);
            raw = (Long) read.invoke(unsafe, base, offset);
            System.out.println("raw initial clock positive: " + (raw > 0));
        }
        clock.setAccessible(true);
        long before = clock.getLong(null);
        if (read != null) {
            long rawAfter = (Long) read.invoke(unsafe, base, offset);
            // A collection between reads may advance the clock. The reflected
            // value must still lie between the two real Unsafe field reads.
            System.out.println("raw clock matches Java: " + (raw <= before && before <= rawAfter));
        }
        SoftReference<Object> reference = new SoftReference<>(new Object());
        Field timestamp = owner.getDeclaredField("timestamp");
        timestamp.setAccessible(true);
        long stamp = timestamp.getLong(reference);
        long after = clock.getLong(null);
        System.out.println("initial clock positive: " + (before > 0 && before >= raw));
        System.out.println("constructor on master clock: " + (stamp > 0 && stamp >= before && stamp <= after));
    }
}
