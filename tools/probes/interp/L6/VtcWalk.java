// Virtual-thread continuations: `Thread.getStackTrace()` of an UNMOUNTED
// virtual thread (parked in `CountDownLatch.await`) is its continuation's
// frames, innermost first (`VirtualThread.tryGetStackTrace` ->
// `Continuation.getStackTrace` -> a `StackWalker` on the continuation).
// HotSpot 25: `n=8 top=park bottom=run`.
//
//   javac -d /tmp/vtc tools/probes/interp/L6/VtcWalk.java
//   java     -cp /tmp/vtc VtcWalk
//   cratonvm -cp /tmp/vtc VtcWalk
//   cratonvm --compatible -cp /tmp/vtc VtcWalk
import java.util.concurrent.CountDownLatch;
public class VtcWalk {
    public static void main(String[] a) throws Exception {
        CountDownLatch in = new CountDownLatch(1), out = new CountDownLatch(1);
        Thread t = Thread.ofVirtual().start(() -> { in.countDown(); try { out.await(); } catch (InterruptedException e) {} });
        in.await();
        while (t.getState() != Thread.State.WAITING) Thread.sleep(1);
        StackTraceElement[] st = t.getStackTrace();
        System.out.println("n=" + st.length + " top=" + st[0].getMethodName() + " bottom=" + st[st.length - 1].getMethodName());
        out.countDown(); t.join();
    }
}
