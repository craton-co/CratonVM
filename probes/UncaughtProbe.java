/**
 * Isolates the uncaught-exception handler chain, with no GC involvement at all.
 *
 * Found while chasing a "double fault" that looked like a GC problem: a thread
 * died with an OutOfMemoryError and `dispatchUncaughtException` died with it,
 * so the log named neither exception. The second exception turned out to be an
 * NPE with nothing to do with the heap -- CratonVM's registered
 * `Thread.getUncaughtExceptionHandler` native returned null where the JDK
 * returns the thread's ThreadGroup, and `Thread.dispatchUncaughtException` is
 * literally `getUncaughtExceptionHandler().uncaughtException(this, e)`.
 *
 * Expected on HotSpot 25 and on CratonVM after the fix:
 *
 *   group=java.lang.ThreadGroup[name=main,maxpri=10]
 *   ueh=java.lang.ThreadGroup[name=main,maxpri=10]
 *   default=null
 *   Exception in thread "Thread-0" java.lang.IllegalStateException: boom
 *       at UncaughtProbe.lambda$main$0(UncaughtProbe.java:...)
 *   joined, still alive
 *
 * The failing shape printed `ueh=null` and then an NPE naming
 * `getUncaughtExceptionHandler()` instead of the stack trace.
 */
public class UncaughtProbe {
    public static void main(String[] a) throws Exception {
        Thread t = new Thread(() -> { throw new IllegalStateException("boom"); });
        System.out.println("group=" + t.getThreadGroup());
        System.out.println("ueh=" + t.getUncaughtExceptionHandler());
        System.out.println("default=" + Thread.getDefaultUncaughtExceptionHandler());
        t.start();
        t.join();
        System.out.println("joined, still alive");
    }
}
