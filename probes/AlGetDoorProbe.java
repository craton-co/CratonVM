import java.util.ArrayList;
import java.util.List;

/**
 * Prices the `java.util.ArrayList` read accessors against the raw-array floor,
 * for Term 2 of the ConfigurationPropertySources page: `ArrayList.get`'s
 * registered native measured 6.3x its own JDK bytecode under the `--jdk-only`
 * retirement dial, and the page names making that the default as its next step.
 *
 * # Two shapes this probe deliberately avoids
 *
 * **No lambda anywhere.** A first cut ran each arm through an
 * `IntFunction<Object>` so the timing harness could be shared. That measured the
 * lambda: the raw-array FLOOR read 248 ns/call and the `ArrayList.get` rung
 * 1190, against a real floor of a couple of ns — and the `--jdk-only`
 * ArrayList-only dial, which is supposed to be worth 6.3x here, moved the rung
 * by 3%. Every arm now has its OWN timed loop in its OWN method, so each call
 * site is monomorphic and directly bound.
 *
 * **Every arm is warmed before any arm is timed.** Compile order binds a call
 * site permanently on this VM, so a rung whose loop is built before its callee
 * warms measures the cold arm forever (the discipline `NativeFunnelFloorProbe`
 * states and the reason it states it).
 *
 *   cratonvm --java-home <jdk25> -cp <out> AlGetDoorProbe [reps] [len]
 */
public final class AlGetDoorProbe {

    static int sink;
    static Object osink;
    static String[] ARR;
    static List<String> LIST;
    static int MASK;

    static long plainLoop(int reps) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) { osink = ARR[0]; }
        return System.nanoTime() - t0;
    }

    /** The FLOOR: a raw reference-array index read. */
    static long arrGetLoop(int reps) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) { osink = ARR[i & MASK]; }
        return System.nanoTime() - t0;
    }

    /** The rung under test. */
    static long alGetLoop(int reps) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) { osink = LIST.get(i & MASK); }
        return System.nanoTime() - t0;
    }

    static long alSizeLoop(int reps) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) { sink += LIST.size(); }
        return System.nanoTime() - t0;
    }

    static long alIsEmptyLoop(int reps) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) { if (LIST.isEmpty()) { sink++; } }
        return System.nanoTime() - t0;
    }

    static long alIterLoop(int reps, int len) {
        long t0 = System.nanoTime();
        for (int i = 0; i < reps / len; i++) { for (String s : LIST) { if (s != null) { sink++; } } }
        return System.nanoTime() - t0;
    }

    static void report(String name, long ns, int reps) {
        System.out.printf("ALGET %-10s reps=%d ns/call=%.1f%n", name, reps, (double) ns / reps);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int len  = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        MASK = len - 1;
        if ((len & MASK) != 0) throw new IllegalArgumentException("len must be a power of two");

        ARR = new String[len];
        LIST = new ArrayList<>(len);
        for (int i = 0; i < len; i++) { String s = "e" + i; ARR[i] = s; LIST.add(s); }

        // Warm EVERY arm before ANY of them is timed.
        for (int w = 0; w < 3; w++) {
            plainLoop(100_000); arrGetLoop(100_000); alGetLoop(100_000);
            alSizeLoop(100_000); alIsEmptyLoop(100_000); alIterLoop(100_000, len);
        }

        report("plain",   plainLoop(reps),        reps);
        report("arrGet",  arrGetLoop(reps),       reps);
        report("alGet",   alGetLoop(reps),        reps);
        report("alSize",  alSizeLoop(reps),       reps);
        report("alEmpty", alIsEmptyLoop(reps),    reps);
        report("alIter",  alIterLoop(reps, len),  reps);
        System.out.println("ALGET sink=" + sink + " osink=" + (osink != null));
    }
}
