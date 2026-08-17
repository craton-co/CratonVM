import io.netty.util.concurrent.DefaultThreadFactory;
import java.security.Permission;
import java.util.concurrent.atomic.AtomicInteger;

public class FirstTouch {
    static long t0 = System.nanoTime();
    static void mark(String s) { System.out.printf("%8.1f ms  %s%n", (System.nanoTime()-t0)/1e6, s); }
    public static void main(String[] a) throws Exception {
        mark("main entry");
        if (!"0".equals(System.getProperty("sm","1"))) System.setSecurityManager(new SecurityManager() {
            @Override public void checkAccess(ThreadGroup g) {
                ThreadGroup src = Thread.currentThread().getThreadGroup();
                if (src != null) { if (!src.parentOf(g)) throw new SecurityException("nope"); super.checkAccess(g); }
            }
            @Override public void checkPermission(Permission p) { }
        });
        mark("setSecurityManager");
        AtomicInteger counter = new AtomicInteger();
        Runnable task = counter::incrementAndGet;
        Thread first = new Thread(new ThreadGroup("brother"), () -> {
            mark("  [brother] enter");
            DefaultThreadFactory f = new DefaultThreadFactory("test", false, Thread.NORM_PRIORITY, null);
            mark("  [brother] new DefaultThreadFactory");
            Thread t = f.newThread(task);
            mark("  [brother] newThread");
            t.start();
            mark("  [brother] t.start");
            try { t.join(); } catch (InterruptedException e) { }
            mark("  [brother] t.join");
        });
        mark("built first");
        first.start(); first.join();
        mark("first done");
        Thread second = new Thread(new ThreadGroup("sister"), () -> {
            mark("  [sister] enter");
            DefaultThreadFactory f = new DefaultThreadFactory("test2", false, Thread.NORM_PRIORITY, null);
            Thread t = f.newThread(task);
            t.start();
            try { t.join(); } catch (InterruptedException e) { }
            mark("  [sister] t.join");
        });
        second.start(); second.join();
        mark("second done counter=" + counter.get());
        System.exit(0);
    }
}
