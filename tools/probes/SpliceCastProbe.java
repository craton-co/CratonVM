import java.util.ArrayList;
import java.util.List;

/** Stand-in for the round-9 probe of the same name: a `List.get` indexing loop
 *  whose body is a checkcast, run from a compiled (not `main`) method. */
public class SpliceCastProbe {
    static List<Object> batch = new ArrayList<>();
    static long sink;

    static long step(int n) {
        long s = 0;
        int m = batch.size();
        for (int i = 0; i < n; i++) {
            Object o = batch.get(i % m);
            s += ((Integer) o).intValue();
        }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        for (int i = 0; i < 16; i++) batch.add(Integer.valueOf(i));
        for (int r = 0; r <= rounds; r++) {
            long t0 = System.nanoTime();
            sink = step(n);
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("r" + r + " step " + ms + " ms chk=" + sink);
        }
    }
}
