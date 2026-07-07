import java.lang.reflect.Method;
import java.util.concurrent.locks.LockSupport;

public class TidProbe {
    public static void main(String[] args) throws Exception {
        Method m = LockSupport.class.getDeclaredMethod("getThreadId", Thread.class);
        m.setAccessible(true);
        check(m, Thread.currentThread());
        for (int i = 0; i < 3; i++) {
            Thread t = new Thread(() -> {
                try {
                    check(m, Thread.currentThread());
                } catch (Exception e) {
                    System.out.println("ERR " + e);
                }
            }, "probe-" + i);
            t.start();
            t.join();
        }
        System.out.println("PROBE_DONE");
    }

    static void check(Method m, Thread t) throws Exception {
        long real = t.threadId();
        long viaUnsafe = (Long) m.invoke(null, t);
        System.out.println("thread=" + t.getName() + " threadId()=" + real
                + " LockSupport.getThreadId=" + viaUnsafe
                + (real == viaUnsafe ? " MATCH" : " *** MISMATCH ***"));
    }
}
