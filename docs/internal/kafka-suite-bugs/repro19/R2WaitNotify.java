// Repro 2: intrinsic monitor wait()/notify() cross-thread handoff. Lower-level
// than R1 — tests the VM's Monitor::wait / Monitor::notify directly, with no
// java.util.concurrent in the way. PASS = notify woke the waiter.
public class R2WaitNotify {
    static final Object lock = new Object();
    static volatile boolean ready = false;
    static volatile boolean woke = false;

    public static void main(String[] args) throws Exception {
        System.out.println("R2 START");
        Thread waiter = new Thread(() -> {
            synchronized (lock) {
                while (!ready) {
                    try { lock.wait(); } catch (InterruptedException e) { return; }
                }
                woke = true;
            }
        }, "waiter");
        waiter.start();
        Thread.sleep(300);                 // let waiter reach wait()
        synchronized (lock) {
            ready = true;
            lock.notify();                 // wake it
        }
        waiter.join(10_000);
        if (waiter.isAlive() || !woke) {
            System.out.println("R2 RESULT=HANG (wait never woke; woke=" + woke + ")");
            System.out.flush();
            Runtime.getRuntime().halt(2);
        }
        System.out.println("R2 RESULT=PASS");
        System.out.flush();
    }
}
