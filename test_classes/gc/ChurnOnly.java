// Control for NioChannelChurn: the SAME allocation shape and the same
// allocator threads, with NO file I/O at all. If this crashes too, the
// remaining producer is not the nio provider natives.
import java.util.ArrayList;
public class ChurnOnly {
    static ArrayList<Object> retained = new ArrayList<>();
    static volatile boolean running = true;
    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            byte[] junk = new byte[4096];
            junk[i % junk.length] = (byte) i;
            if (junk[0] == 77 && junk[1] == 88) System.out.println("unreachable");
        }
        retained.add(new long[512]);
        if (retained.size() > 32) retained.subList(0, 16).clear();
    }
    public static void main(String[] a) throws Exception {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 300;
        int per = a.length > 1 ? Integer.parseInt(a[1]) : 200;
        int threads = a.length > 2 ? Integer.parseInt(a[2]) : 4;
        ArrayList<Thread> ts = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            Thread t = new Thread(() -> {
                ArrayList<Object> local = new ArrayList<>();
                while (running) {
                    for (int k = 0; k < 64; k++) local.add(new byte[2048]);
                    if (local.size() > 512) local.subList(0, 256).clear();
                }
            }, "allocator");
            t.setDaemon(true); ts.add(t); t.start();
        }
        for (int r = 0; r < rounds; r++) { churn(per); churn(per); }
        running = false;
        for (Thread t : ts) t.join(2000);
        System.out.println("CHURN_ONLY_DONE rounds=" + rounds);
    }
}
