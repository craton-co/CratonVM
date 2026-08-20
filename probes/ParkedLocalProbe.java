import java.util.ArrayList;
import java.util.List;

/**
 * Does a platform thread that BLOCKS (sleep/wait) across a moving collection
 * come back with its frame locals still naming the objects it had?
 *
 * Each holder thread keeps one object in a local and one in a field, sleeps in
 * a loop while churn threads force collections, and after every sleep asks the
 * held objects who they are. A wrong class, a wrong id, or a NoSuchMethodError
 * is a frame slot that was not fixed up across the pause.
 */
public class ParkedLocalProbe {
    static volatile boolean stop = false;
    static volatile int bad = 0;
    static volatile int checks = 0;

    static final class Marker {
        final int id;
        final String tag;
        Marker(int id) { this.id = id; this.tag = "marker-" + id; }
        int id() { return id; }
        String tag() { return tag; }
    }

    static final class Holder {
        Marker field;
    }

    public static void main(String[] args) throws Exception {
        int holders = Integer.getInteger("holders", 8);
        int churners = Integer.getInteger("churners", 4);
        int rounds = Integer.getInteger("rounds", 400);
        int sleepMs = Integer.getInteger("sleepMs", 3);

        Thread[] churn = new Thread[churners];
        for (int i = 0; i < churners; i++) {
            churn[i] = new Thread(() -> {
                List<Object> keep = new ArrayList<>();
                while (!stop) {
                    keep.add(new byte[2048]);
                    keep.add(new Marker(-1));
                    if (keep.size() > 4000) keep.clear();
                }
            }, "churn-" + i);
            churn[i].setDaemon(true);
            churn[i].start();
        }

        Thread[] ts = new Thread[holders];
        for (int i = 0; i < holders; i++) {
            final int id = i + 1;
            ts[i] = new Thread(() -> {
                Marker local = new Marker(id);
                Holder holder = new Holder();
                holder.field = new Marker(id + 1000);
                for (int r = 0; r < rounds && !stop; r++) {
                    try { Thread.sleep(sleepMs); } catch (InterruptedException e) { return; }
                    checks++;
                    try {
                        if (local.id() != id || !local.tag().equals("marker-" + id)) {
                            bad++;
                            System.out.println("BAD local  thread=" + id + " class=" + local.getClass().getName()
                                    + " id=" + local.id + " tag=" + local.tag);
                        }
                        Marker f = holder.field;
                        if (f.id() != id + 1000 || !f.tag().equals("marker-" + (id + 1000))) {
                            bad++;
                            System.out.println("BAD field  thread=" + id + " class=" + f.getClass().getName()
                                    + " id=" + f.id + " tag=" + f.tag);
                        }
                    } catch (Throwable t) {
                        bad++;
                        System.out.println("BAD throw  thread=" + id + " " + t);
                    }
                }
            }, "holder-" + id);
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        stop = true;
        System.out.println("checks=" + checks + " bad=" + bad);
    }
}
