package cratonvm;
public class TckThread {
    public static int thread_currentThread() { return Thread.currentThread() != null ? 1 : 0; }
    public static int thread_getName() { return Thread.currentThread().getName() != null ? 1 : 0; }
    public static int thread_isAlive() { return Thread.currentThread().isAlive() ? 1 : 0; }
    public static int thread_priority() { int p = Thread.currentThread().getPriority(); return (p >= Thread.MIN_PRIORITY && p <= Thread.MAX_PRIORITY) ? 1 : 0; }
    public static int thread_isDaemon() { /* main thread is not daemon */ return !Thread.currentThread().isDaemon() ? 1 : 0; }
    public static int thread_id() { return Thread.currentThread().getId() > 0 ? 1 : 0; }
    public static int thread_new_name() { Thread t = new Thread("test-thread"); return "test-thread".equals(t.getName()) ? 1 : 0; }
    public static int thread_start_join() {
        try {
            final int[] result = {0};
            Thread t = new Thread(new Runnable() { public void run() { result[0] = 42; } });
            t.start();
            t.join();
            return result[0] == 42 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int thread_sleep() {
        try {
            long before = System.currentTimeMillis();
            Thread.sleep(10);
            long after = System.currentTimeMillis();
            return (after - before) >= 5 ? 1 : 0; // some tolerance
        } catch (Exception e) { return 0; }
    }
    public static int thread_interrupt() {
        Thread t = Thread.currentThread();
        t.interrupt();
        boolean was = Thread.interrupted();
        return was ? 1 : 0;
    }
    public static int thread_state() {
        Thread.State s = Thread.currentThread().getState();
        return s == Thread.State.RUNNABLE ? 1 : 0;
    }
}
