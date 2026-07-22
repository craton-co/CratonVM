// Two thread-creation styles side by side:
//  (1) subclass overriding run()  -> dispatch starts at subclass, no holder.task needed
//  (2) Runnable target            -> base Thread.run() must read holder.task
public class R0Sub {
    static volatile boolean subRan = false;
    static volatile boolean tgtRan = false;

    static class Sub extends Thread {
        public void run() { subRan = true; System.out.println("SUB RAN"); }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("R0Sub START");

        Thread s = new Sub();
        s.start(); s.join(10_000);
        System.out.println("(1) subclass run  -> subRan=" + subRan);

        Thread t = new Thread(() -> { tgtRan = true; System.out.println("TARGET RAN"); }, "tgt");
        t.start(); t.join(10_000);
        System.out.println("(2) Runnable tgt  -> tgtRan=" + tgtRan);

        System.out.flush();
    }
}
