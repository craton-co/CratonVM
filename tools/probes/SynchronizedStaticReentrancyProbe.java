// Standalone repro for a suspected monitor-reentrancy bug found while tracing
// the Tomcat WebSocket test-suite hang: the main thread's stack showed
// java.util.logging.LogManager.demandSystemLogger (static synchronized)
// calling itself recursively (via ClassLoaderLogManager.addLogger's
// parent-logger instantiation, Logger.getLogger(parentName)) and never
// returning. Real JDK monitors are reentrant per-thread by spec; if CratonVM's
// synchronized-static implementation does not recognize a nested call from
// the SAME thread as already holding the class-level monitor, this hangs.
public class SynchronizedStaticReentrancyProbe {
    private static int depth = 0;

    static synchronized void a() {
        depth++;
        System.out.println("a() enter, depth=" + depth);
        if (depth < 3) {
            b();
        }
        System.out.println("a() exit, depth=" + depth);
        depth--;
    }

    // A second synchronized static method on the SAME class -- same monitor
    // (the Class object) as a(). Mirrors demandSystemLogger calling itself
    // through one level of unrelated code (addLocalLogger/getLevelProperty)
    // in between.
    static synchronized void b() {
        System.out.println("b() enter (should already hold the class monitor)");
        a();
        System.out.println("b() exit");
    }

    public static void main(String[] args) throws Exception {
        Thread watchdog = new Thread(() -> {
            try { Thread.sleep(15000); } catch (InterruptedException ignored) { return; }
            System.out.println("WATCHDOG: still not done after 15s -- HANG in reentrant synchronized static calls");
            System.exit(1);
        });
        watchdog.setDaemon(true);
        watchdog.start();

        a();
        System.out.println("main: completed without hanging");

        // Cross-thread contention control: a genuinely different thread
        // trying to enter a()/b() WHILE the main thread holds the monitor
        // should block (this is supposed to happen) and then proceed once
        // main releases it -- confirms the monitor is a real mutex, not
        // merely a no-op, so the reentrant case above is a meaningful test.
        Thread other = new Thread(() -> {
            System.out.println("other-thread: calling a()");
            a();
            System.out.println("other-thread: a() returned");
        });
        other.start();
        other.join(10000);
        if (other.isAlive()) {
            System.out.println("WATCHDOG: other-thread never completed either");
            System.exit(1);
        }
        System.out.println("ALL OK");
    }
}
