/** The interrupt-status rules for a thread that is NEW and for one that has
 *  TERMINATED, asked directly rather than inferred from a sweep row. */
public class ThreadIntrDbg {
    public static void main(String[] a) throws Exception {
        Thread n = new Thread(() -> { });
        System.out.println("NEW before        |" + n.isInterrupted() + "|");
        n.interrupt();
        System.out.println("NEW after         |" + n.isInterrupted() + "|");
        n.interrupt();
        System.out.println("NEW after twice   |" + n.isInterrupted() + "|");
        n.start();
        n.join();
        System.out.println("NEW then ran      |" + n.isInterrupted() + "|");

        Thread d = new Thread(() -> { });
        d.start();
        d.join();
        System.out.println("TERM before       |" + d.isInterrupted() + "|");
        d.interrupt();
        System.out.println("TERM after        |" + d.isInterrupted() + "|");
        System.out.println("TERM state        |" + d.getState() + "|");

        Thread s = new Thread(() -> {
            try { Thread.sleep(5000); } catch (InterruptedException e) { }
        });
        s.start();
        while (s.getState() != Thread.State.TIMED_WAITING) Thread.sleep(5);
        s.interrupt();
        System.out.println("LIVE after        |" + s.isInterrupted() + "|");
        s.join();
        System.out.println("LIVE then dead    |" + s.isInterrupted() + "|");
        System.out.println("DONE ThreadIntrDbg");
    }
}
