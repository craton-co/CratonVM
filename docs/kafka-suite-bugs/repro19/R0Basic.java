// R0: does a second thread run at all, and is cross-thread state visible —
// WITHOUT any blocking/wakeup primitive? Pure Thread.start + join, then a
// busy-poll handoff over a volatile. Isolates "threading works" from
// "cross-thread wakeup works".
public class R0Basic {
    static volatile boolean ran = false;
    static volatile int handoff = 0;

    public static void main(String[] args) throws Exception {
        System.out.println("R0 START");

        // (a) does a plain thread body execute?
        Thread t = new Thread(() -> { ran = true; }, "t");
        t.start();
        t.join(10_000);
        System.out.println("R0 a) thread-ran=" + ran + " alive=" + t.isAlive());

        // (b) busy-poll handoff: child waits for main to set handoff=1 (no
        //     wait/park — pure spin on a volatile), then sets it to 2.
        Thread c = new Thread(() -> {
            long spins = 0;
            while (handoff != 1) { spins++; if (spins > 2_000_000_000L) return; }
            handoff = 2;
        }, "c");
        c.start();
        Thread.sleep(300);
        handoff = 1;                       // main publishes
        c.join(10_000);
        System.out.println("R0 b) handoff=" + handoff + " (expect 2) alive=" + c.isAlive());

        System.out.println("R0 RESULT=" + ((ran && handoff == 2) ? "PASS" : "FAIL"));
        System.out.flush();
    }
}
