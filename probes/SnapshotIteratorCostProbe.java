import java.util.Arrays;
import java.util.Iterator;
import java.util.List;

/**
 * What a snapshot iterator costs to build, so converting the idiom to the real
 * `java.util.Arrays$ArrayItr` has a measured price rather than an asserted one.
 *
 * The fabricated shape was one allocation plus three field writes; the real one
 * is one allocation plus two, so the expectation is that the honest version is
 * not the expensive version. That is exactly the kind of expectation worth
 * checking, since `make_iterator_from_array` runs on every `Arrays.asList`
 * iteration, every `Path` walk, every `EnumSet` drain.
 *
 * Prints a checksum as well as the timing: a run that iterated nothing would
 * otherwise look fast. Two rounds, so a first-round class-load and JIT warmup
 * does not get read as the steady-state cost.
 */
public final class SnapshotIteratorCostProbe {

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        String[] backing = new String[8];
        for (int i = 0; i < backing.length; i++) {
            backing[i] = "e" + i;
        }
        List<String> list = Arrays.asList(backing);

        for (int round = 0; round < 3; round++) {
            long sum = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                // One iterator construction plus a full drain per loop: the
                // construction is what changed, the drain keeps the iterator
                // from being trivially dead.
                Iterator<String> it = list.iterator();
                while (it.hasNext()) {
                    sum += it.next().length();
                }
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("round" + round + " iters=" + iters + " sum=" + sum + " ms=" + ms);
        }
    }
}
