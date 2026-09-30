// Interpreter round i1, wave 16, lane L4 — type-check hot paths.
//
// What to measure: the per-row time below on CratonVM with the JIT off
// (`--nojit`, or CRATONVM_DISABLE_JIT=1), against the previous build,
// interleaving binaries and taking the median of the reps (in-JVM timings on
// this host swing ~3x between reps). Timings go to STDERR (min ns/op per
// row); STDOUT carries only checksums, identical to HotSpot 25:
//   classMono checksum=1000000
//   classPoly checksum=1000000
//   classNeg checksum=0
//   ifaceMono checksum=1000000
//   ifacePoly checksum=1000000
//   ifaceNeg checksum=0
//   arrObject checksum=1000000
//   arrCovar checksum=1000000
//   arrPoly checksum=1000000
//   arrPrim checksum=1000000
//   arrNeg checksum=0
//   classNegPoly checksum=0
//   ifaceNegPoly checksum=0
//   ladder3 checksum=1999999
//   total checksum=9999999
//
// What the wave-16 stage should move (i11-L2 array receiver memo,
// `CastSite::positive_array`):
//   arrObject — `(Object[]) x` on a String[]: `checkcast [Ljava/lang/Object;`,
//               the erasure of every `(T[]) x`. Before: no cast-site entry at
//               all (trivial target, never filled), so a class_manager read
//               for the name plus the array verdict per execution. After: one
//               table probe and two header loads.
//   arrCovar  — `(CharSequence[]) x` on a String[] (non-trivial target):
//               before, every execution fell to the full path (`unusable`);
//               after, answered from the array memo.
//   arrPrim   — `instanceof int[]` / `checkcast int[]` on an int[].
//   arrPoly   — String[] and Integer[] alternating at one `(Object[])` site:
//               the one-entry memo thrashes; a regression guard (must not be
//               slower than before).
//   arrNeg    — `instanceof String[]` on an Integer[]: refusals are not
//               memoised; unchanged path, the baseline for a negative stage.
// The class / interface rows are the controls (positive memo, negative memo
// and the published supers closure), which this stage must leave unchanged.
//
// What the wave-20 stage should move (i16-L4 proposal, lane i20-L5: two
// negative receiver slots per `instanceof` site, `CastSite::negative_receivers`):
//   classNegPoly — `instanceof Base` refusing Other and Other2 alternately:
//                  before, the one-slot negative memo thrashed and every
//                  execution ran the full path (about six lock acquisitions
//                  and a name read); after, both refusals are memo hits.
//   ifaceNegPoly — the same against an interface target.
//   ladder3      — a three-rung `instanceof` ladder (SubA / SubB / Other)
//                  whose receiver rotates through the three classes: the
//                  first rung refuses two classes, the second one.
// classNeg / ifaceNeg are the monomorphic controls (must not move). Under
// `CRATONVM_DBG_FIELD_SITE=1`, `neg_hit=` should grow by about 3M per rep
// over the three new rows and `unusable=` should stop growing with them;
// `CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO=1` withdraws every negative memo (the
// A/B lever for all negative rows at once).
// `CRATONVM_DBG_FIELD_SITE=1` prints `cast: hit= neg_hit= array_hit= ...` at
// exit — read it before quoting a number (array_hit=0 means the memo never
// fired).
public class TypeCheckBench {
    static class Base { int v() { return 1; } }
    static final class SubA extends Base {}
    static final class SubB extends Base {}
    interface Shape { int size(); }
    static final class Sq implements Shape { public int size() { return 1; } }
    static final class Ci implements Shape { public int size() { return 1; } }
    static final class Other {}
    static final class Other2 {}

    static final int N = 1_000_000;
    static final int REPS = 5;

    static int classMono(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Base) s += ((Base) o).v();
        }
        return s;
    }

    static int classNeg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (xs[i & 1] instanceof Base) s++;
        }
        return s;
    }

    static int iface(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Shape) s += ((Shape) o).size();
        }
        return s;
    }

    static int ifaceNeg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (xs[i & 1] instanceof Shape) s++;
        }
        return s;
    }

    static int arrObject(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Object[]) s += ((Object[]) o).length;
        }
        return s;
    }

    static int arrCovar(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof CharSequence[]) s += ((CharSequence[]) o).length;
        }
        return s;
    }

    static int arrPrim(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof int[]) s += ((int[]) o).length;
        }
        return s;
    }

    static int arrNeg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (xs[i & 1] instanceof String[]) s++;
        }
        return s;
    }

    static int ladder3(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i % 3];
            if (o instanceof SubA) s += 1;
            else if (o instanceof SubB) s += 2;
            else if (o instanceof Other) s += 3;
        }
        return s;
    }

    interface Row { int run(Object[] xs); }

    static long total;

    static void row(String name, Row r, Object[] xs) {
        int checksum = 0;
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            int c = r.run(xs);
            long dt = System.nanoTime() - t0;
            if (dt < best) best = dt;
            if (rep == 0) checksum = c;
            else if (c != checksum) checksum = -1;
        }
        total += checksum;
        System.out.println(name + " checksum=" + checksum);
        System.err.printf("%-10s %8.2f ns/op (min of %d)%n", name, (double) best / N, REPS);
    }

    public static void main(String[] args) {
        row("classMono", TypeCheckBench::classMono, new Object[] { new SubA(), new SubA() });
        row("classPoly", TypeCheckBench::classMono, new Object[] { new SubA(), new SubB() });
        row("classNeg", TypeCheckBench::classNeg, new Object[] { new Other(), new Other() });
        row("ifaceMono", TypeCheckBench::iface, new Object[] { new Sq(), new Sq() });
        row("ifacePoly", TypeCheckBench::iface, new Object[] { new Sq(), new Ci() });
        row("ifaceNeg", TypeCheckBench::ifaceNeg, new Object[] { new Other(), new Other() });
        String[] strings = { "a" };
        Integer[] ints = { 1 };
        row("arrObject", TypeCheckBench::arrObject, new Object[] { strings, strings });
        row("arrCovar", TypeCheckBench::arrCovar, new Object[] { strings, strings });
        row("arrPoly", TypeCheckBench::arrObject, new Object[] { strings, ints });
        int[] prim = { 7 };
        row("arrPrim", TypeCheckBench::arrPrim, new Object[] { prim, prim });
        row("arrNeg", TypeCheckBench::arrNeg, new Object[] { ints, ints });
        row("classNegPoly", TypeCheckBench::classNeg, new Object[] { new Other(), new Other2() });
        row("ifaceNegPoly", TypeCheckBench::ifaceNeg, new Object[] { new Other(), new Other2() });
        row("ladder3", TypeCheckBench::ladder3, new Object[] { new SubA(), new SubB(), new Other() });
        System.out.println("total checksum=" + total);
    }
}
