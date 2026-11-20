package cratonvm;
import java.util.concurrent.atomic.*;
public class TckAtomic {
    public static int atomic_int_get_set() { AtomicInteger ai = new AtomicInteger(0); ai.set(42); return ai.get() == 42 ? 1 : 0; }
    public static int atomic_int_getAndSet() { AtomicInteger ai = new AtomicInteger(5); return ai.getAndSet(10) == 5 && ai.get() == 10 ? 1 : 0; }
    public static int atomic_int_cas() { AtomicInteger ai = new AtomicInteger(5); return ai.compareAndSet(5, 10) && ai.get() == 10 ? 1 : 0; }
    public static int atomic_int_cas_fail() { AtomicInteger ai = new AtomicInteger(5); return !ai.compareAndSet(3, 10) && ai.get() == 5 ? 1 : 0; }
    public static int atomic_int_incr() { AtomicInteger ai = new AtomicInteger(0); return ai.incrementAndGet() == 1 ? 1 : 0; }
    public static int atomic_int_addAndGet() { AtomicInteger ai = new AtomicInteger(10); return ai.addAndGet(5) == 15 ? 1 : 0; }
    public static int atomic_long_basic() { AtomicLong al = new AtomicLong(0L); al.set(100L); return al.get() == 100L ? 1 : 0; }
    public static int atomic_long_cas() { AtomicLong al = new AtomicLong(5L); return al.compareAndSet(5L, 10L) && al.get() == 10L ? 1 : 0; }
    public static int atomic_ref_basic() { AtomicReference<String> ar = new AtomicReference<>("hello"); return "hello".equals(ar.get()) ? 1 : 0; }
    public static int atomic_ref_cas() { AtomicReference<String> ar = new AtomicReference<>("a"); return ar.compareAndSet("a", "b") && "b".equals(ar.get()) ? 1 : 0; }
    public static int atomic_boolean() { AtomicBoolean ab = new AtomicBoolean(false); ab.set(true); return ab.get() ? 1 : 0; }
}
