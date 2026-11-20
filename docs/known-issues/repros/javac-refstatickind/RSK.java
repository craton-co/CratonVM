import java.util.ArrayList;
import java.util.List;

/**
 * Shape-only reproducer for the javac ClassCastException seen while compiling
 * Spring AOT-generated sources:
 *
 *   java.lang.ClassCastException: com.sun.tools.javac.code.Symbol$MethodSymbol
 *     cannot be cast to com.sun.tools.javac.comp.Resolve$ReferenceLookupResult$StaticKind
 *       at com.sun.tools.javac.comp.Resolve$ReferenceLookupResult.staticKind(Resolve.java:3317)
 *
 * Resolve.java:3313-3317 is a single stream pipeline:
 *
 *   return resolutionContext.candidates.stream()
 *           .filter(c -> c.isApplicable() && c.step == resolutionContext.step)
 *           .map(c -> StaticKind.from(c.sym))
 *           .reduce(StaticKind::reduce)
 *           .orElse(StaticKind.UNDEFINED);
 *
 * A MethodSymbol arriving at that checkcast means the value that came out of
 * the pipeline was `c.sym` -- so either the `.map` step behaved as identity or
 * `StaticKind.from(s)` returned its own argument.
 *
 * This file replicates that shape with local classes so it runs in a second
 * instead of driving a 55-minute Spring test.  It prints one line per case;
 * every line must read OK.
 */
public class RSK {

    static final class Sym {
        final boolean stat;
        final String name;

        Sym(String name, boolean stat) {
            this.name = name;
            this.stat = stat;
        }

        boolean isStatic() {
            return stat;
        }

        @Override
        public String toString() {
            return "Sym(" + name + "," + (stat ? "static" : "instance") + ")";
        }
    }

    static final class Cand {
        final int step;
        final Sym sym;
        final Object mtype;

        Cand(int step, Sym sym, Object mtype) {
            this.step = step;
            this.sym = sym;
            this.mtype = mtype;
        }

        boolean isApplicable() {
            return mtype != null;
        }
    }

    enum Kind {
        STATIC,
        NON_STATIC,
        BOTH,
        UNDEFINED;

        static Kind from(Sym s) {
            return s.isStatic() ? STATIC : NON_STATIC;
        }

        static Kind reduce(Kind sk1, Kind sk2) {
            if (sk1 == UNDEFINED) {
                return sk2;
            } else if (sk2 == UNDEFINED) {
                return sk1;
            } else {
                return sk1 == sk2 ? sk1 : BOTH;
            }
        }
    }

    /** Byte-for-byte the shape of Resolve$ReferenceLookupResult.staticKind. */
    static Kind staticKind(List<Cand> candidates, int step) {
        return candidates.stream()
                .filter(c -> c.isApplicable() && c.step == step)
                .map(c -> Kind.from(c.sym))
                .reduce(Kind::reduce)
                .orElse(Kind.UNDEFINED);
    }

    static void check(String label, List<Cand> cands, int step, Kind want) {
        try {
            Kind got = staticKind(cands, step);
            System.out.println(label + (got == want ? " OK" : " WRONG got=" + got + " want=" + want));
        } catch (Throwable t) {
            System.out.println(label + " THREW " + t);
        }
    }

    /**
     * Same pipeline reached through a raw Object so the checkcast the compiler
     * emits at the call site is the only thing that can fail. Isolates "the
     * pipeline produced the wrong element type" from "the assignment failed".
     */
    static void raw(String label, List<Cand> cands, int step) {
        try {
            Object got = cands.stream()
                    .filter(c -> c.isApplicable() && c.step == step)
                    .map(c -> Kind.from(c.sym))
                    .reduce(Kind::reduce)
                    .orElse(Kind.UNDEFINED);
            String cls = got == null ? "null" : got.getClass().getName();
            System.out.println(label + (got instanceof Kind ? " OK " : " WRONGTYPE ") + cls + " value=" + got);
        } catch (Throwable t) {
            System.out.println(label + " THREW " + t);
        }
    }

    static List<Cand> list(int step, Cand... cs) {
        List<Cand> l = new ArrayList<>();
        for (Cand c : cs) {
            l.add(c);
        }
        return l;
    }

    public static void main(String[] args) {
        Sym inst = new Sym("m", false);
        Sym stat = new Sym("m", true);
        Object mt = new Object();

        // 0 elements survive the filter -> orElse supplies UNDEFINED.
        check("empty", list(0), 0, Kind.UNDEFINED);
        check("all-filtered-out", list(0, new Cand(1, inst, mt)), 0, Kind.UNDEFINED);
        check("not-applicable", list(0, new Cand(0, inst, null)), 0, Kind.UNDEFINED);

        // 1 element -> reduce returns it WITHOUT calling the accumulator.
        // This is the case where a no-op `.map` shows up as the raw element.
        check("one-instance", list(0, new Cand(0, inst, mt)), 0, Kind.NON_STATIC);
        check("one-static", list(0, new Cand(0, stat, mt)), 0, Kind.STATIC);

        // 2+ elements -> the accumulator (a method reference) runs.
        check("two-same", list(0, new Cand(0, inst, mt), new Cand(0, inst, mt)), 0, Kind.NON_STATIC);
        check("two-mixed", list(0, new Cand(0, inst, mt), new Cand(0, stat, mt)), 0, Kind.BOTH);
        check("three-mixed", list(0, new Cand(0, stat, mt), new Cand(0, inst, mt), new Cand(0, stat, mt)), 0, Kind.BOTH);

        // Mixed with filtered-out entries interleaved, as javac's real
        // candidate list looks after several resolution phases.
        check("interleaved", list(0,
                new Cand(1, stat, mt),
                new Cand(0, inst, mt),
                new Cand(0, stat, null),
                new Cand(0, stat, mt),
                new Cand(2, inst, mt)), 0, Kind.BOTH);

        raw("raw-one", list(0, new Cand(0, inst, mt)), 0);
        raw("raw-two", list(0, new Cand(0, inst, mt), new Cand(0, stat, mt)), 0);
        raw("raw-empty", list(0), 0);

        // Hot loop: the real failure appears deep into a javac run, so give the
        // JIT enough iterations to compile the pipeline and its lambdas.
        List<Cand> hot = list(0, new Cand(0, inst, mt), new Cand(0, stat, mt));
        int bad = 0;
        Throwable first = null;
        for (int i = 0; i < 200000; i++) {
            try {
                if (staticKind(hot, 0) != Kind.BOTH) {
                    bad++;
                }
            } catch (Throwable t) {
                bad++;
                if (first == null) {
                    first = t;
                }
            }
        }
        System.out.println("hot-200k " + (bad == 0 ? "OK" : "BAD n=" + bad + " first=" + first));
    }
}
