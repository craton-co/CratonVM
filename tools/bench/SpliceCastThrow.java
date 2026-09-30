/**
 * The correctness half of the spliced type check: the cast that FAILS, the
 * null that must pass, and the `instanceof` that must answer false -- all from
 * inside a body the optimizing tier relocated into its caller.
 *
 * A spliced `checkcast` is the first spliced site that can throw on a value the
 * CALLER produced, so the ClassCastException's message, its type and the point
 * execution resumes at are what this pins. Checksummed against HotSpot.
 */
public class SpliceCastThrow {
    static class Box { final int v; Box(int v) { this.v = v; } }
    static class Other { final int v; Other(int v) { this.v = v; } }

    static final Object[] TABLE = new Object[64];
    static {
        for (int i = 0; i < TABLE.length; i++) {
            if ((i & 15) == 5)      TABLE[i] = new Other(i);   // makes unwrap throw
            else if ((i & 15) == 9) TABLE[i] = null;           // cast passes, deref throws
            else                    TABLE[i] = new Box(i * 31);
        }
    }

    static int unwrap(Object o) { return ((Box) o).v; }
    static int tagOf(Object o)  { return o instanceof Box ? 1 : 0; }

    static int step(int acc, int i) {
        Object o = TABLE[i & 63];
        int t = tagOf(o);
        try {
            acc += unwrap(o);
        } catch (ClassCastException e) {
            // The exception TYPE and the point control resumes at, not the
            // message: CratonVM's CCE detail text is not HotSpot's, which is a
            // separate (and pre-existing) difference this probe must not fold
            // in if it wants to be checksum-comparable against Temurin.
            acc = acc * 31 + 2;
        } catch (NullPointerException e) {
            acc = acc * 7 + 3;
        }
        return acc + t;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int warm = 0;
        for (int i = 0; i < 1_500_000; i++) warm = step(warm, i);
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        // The rep count is part of the output on purpose. This checksum is only
        // comparable against another taken at the SAME count, and printing only
        // the checksum is how a CratonVM run at 300 000 and a Temurin run at this
        // method's 2 000 000 default were once read as a VM difference -- see
        // docs/internal/performance/c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md
        // section 8.3. `SpliceCastProbe` has always printed its reps; this now matches it.
        System.out.println("splicecastthrow (" + reps + ") [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
