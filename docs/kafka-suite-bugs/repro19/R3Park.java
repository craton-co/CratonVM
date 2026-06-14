// Repro 3: LockSupport.park()/unpark() cross-thread handoff — the primitive
// that real java.util.concurrent AQS (CRATONVM_REAL_AQS=1) bottoms out on.
// PASS = unpark woke the parked thread.
import java.util.concurrent.locks.LockSupport;

public class R3Park {
    static volatile boolean woke = false;

    public static void main(String[] args) throws Exception {
        System.out.println("R3 START");
        final Thread main = Thread.currentThread();
        Thread parker = new Thread(() -> {
            LockSupport.park();            // block until unparked
            woke = true;
        }, "parker");
        parker.start();
        Thread.sleep(300);                 // let parker reach park()
        LockSupport.unpark(parker);        // wake it
        parker.join(10_000);
        if (parker.isAlive() || !woke) {
            System.out.println("R3 RESULT=HANG (park never woke; woke=" + woke + ")");
            System.out.flush();
            Runtime.getRuntime().halt(2);
        }
        System.out.println("R3 RESULT=PASS");
        System.out.flush();
    }
}
