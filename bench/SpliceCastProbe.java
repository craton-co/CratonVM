import java.util.ArrayList;
import java.util.List;

/**
 * SpliceCastProbe -- the callee shape the optimizing tier refused as
 * `checkcast/instanceof`.
 *
 * `asBox` is the accessor every generic container read goes through once the
 * container is untyped: take an Object, check it, return it typed. javac emits
 * `checkcast` for the cast and the JIT's own survey counted 306 events on this
 * pair -- the largest single whole-method refusal it found, more than every
 * opcode gap combined. `kindOf` is the `instanceof` half.
 *
 * Both were refused for splicing, so a hot method reading a container paid a
 * real call per element for a body that is one type check and a return.
 *
 * The list is built once and read in the timed loop, so the measurement is the
 * read path and not allocation. Deterministic and checksummed, like every other
 * harness here: a fast wrong answer is a bug, not a result.
 *
 * MEASURED 2026-09-09. The gate does what it says -- spliced bodies 2 -> 6 --
 * and the throughput here is dominated by something else: the optimizing body
 * is ~3x slower than the single-pass one in BOTH arms (931-1065 ms against
 * 306-443 ms). `ir blind dispatches: own_code=0 in_splice=1` names the suspect,
 * a surviving `invokevirtual` (ArrayList.elementData) inside a relocated body
 * that got neither a direct bind nor its MIC/PIC. SpliceCastArrayProbe is the
 * attribution: same accessors, no container, and there the optimizing body is
 * the faster one.
 *
 * Usage: SpliceCastProbe [reps]     default 4,000,000
 */
public class SpliceCastProbe {
    static final class Box {
        final int v;
        Box(int v) { this.v = v; }
        int value() { return v; }
    }

    // `checkcast` behind an accessor -- the shape under test.
    static Box asBox(Object o) {
        return (Box) o;
    }

    // `instanceof` behind an accessor.
    static int kindOf(Object o) {
        return o instanceof Box ? 1 : 0;
    }

    static int step(List<Object> xs, int acc, int i) {
        Object o = xs.get(i & 15);
        return acc * 31 + asBox(o).value() + kindOf(o);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        List<Object> xs = new ArrayList<>();
        for (int i = 0; i < 16; i++) xs.add(new Box(i * 7 + 1));
        // Warm past the tier manager's threshold AND far enough for the
        // background compiler to publish before the timed loop starts; see
        // SpliceStaticProbe's header on why the shorter warm-up mixes two
        // bodies into one median.
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(xs, warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(xs, acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicecast (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
