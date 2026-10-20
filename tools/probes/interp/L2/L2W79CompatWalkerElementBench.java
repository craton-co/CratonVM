// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L2 -- what a walked frame's
// StackTraceElement and its class name cost under --compatible, on frames
// through 12 distinct classes. Bench for
// docs/internal/fixed-bugs/interpreter-i77-L2-proposal-compatible-walker-element-cost-KEPT-OFF-20261111.md
// and docs/internal/fixed-bugs/interpreter-i77-L2-proposal-compatible-frame-class-name-without-reentry-KEPT-OFF-20261111.md.
//
// Switches (opt-in, default OFF; --compatible over a real JDK only):
//   CRATONVM_COMPAT_SFI_ELEMENT_JDK_BODY=1  StackFrameInfo.toStackTraceElement()
//       runs its JDK body (synchronized memo) instead of the lang_stackwalker
//       Bridge, and StackTraceElement.of(StackFrameInfo) is the reviewed
//       Intrinsic --jdk-only uses (rows walker-*-ste-*).
//   CRATONVM_COMPAT_SFI_CLASS_NAME_DIRECT=1  StackFrameInfo.getClassName()'s
//       native answers Class.getName()'s memo before re-entering getName()
//       (row walker-frames-12).
//
// Rows (one untimed warm-up pass, then REPS timed passes; the MINIMUM is
// printed). Every row runs at the bottom of a chain of 12 distinct classes:
//   walker-frames-12        walk, StackFrame.getClassName() per frame (no element)
//   walker-to-ste-12        walk, toStackTraceElement() per frame through a
//                           stream map (the wave-76/77 row)
//   walker-foreach-ste-12   walk, toStackTraceElement() per frame from a
//                           forEach(Consumer): no stream pipeline per element
//   walker-ste-again-12     one walk's frames kept, toStackTraceElement() asked
//                           again on each (the memo hit)
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W79CompatWalkerElementBench <row>`. Each row-set runs well under 60 s.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima:
//   $B --compatible --java-home $J/.. -cp out L2W79CompatWalkerElementBench                                           # (a) off
//   env CRATONVM_COMPAT_SFI_ELEMENT_JDK_BODY=1 $B --compatible --java-home $J/.. -cp out L2W79CompatWalkerElementBench # (b) element
//   env CRATONVM_COMPAT_SFI_CLASS_NAME_DIRECT=1 $B --compatible --java-home $J/.. -cp out L2W79CompatWalkerElementBench # (c) name
//   (a), (b), (c) with --nojit; $B --java-home $J/.. -cp out L2W79CompatWalkerElementBench (--jdk-only reference)
//   $J/java -cp out L2W79CompatWalkerElementBench                                                                     # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// walker-frames-12 checksum=2000
// walker-to-ste-12 checksum=2000
// walker-foreach-ste-12 checksum=2000
// walker-ste-again-12 checksum=2000
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.function.Predicate;
import java.util.stream.Stream;

public class L2W79CompatWalkerElementBench {
    static final int ITERS_WALK = 2_000;
    static final int REPS = 5;

    static final int WALKER_FRAMES = 0;
    static final int WALKER_TO_STE = 1;
    static final int WALKER_FOREACH_STE = 2;
    static final int WALKER_STE_AGAIN = 3;

    static final StackWalker WALKER = StackWalker.getInstance();

    // Named functions (no lambdas).
    static class Frames implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.filter(new Predicate<StackWalker.StackFrame>() {
                @Override
                public boolean test(StackWalker.StackFrame f) {
                    return f.getClassName().endsWith("$K0");
                }
            }).count();
        }
    }

    static class Elements implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.map(new Function<StackWalker.StackFrame, StackTraceElement>() {
                @Override
                public StackTraceElement apply(StackWalker.StackFrame f) {
                    return f.toStackTraceElement();
                }
            }).filter(new Predicate<StackTraceElement>() {
                @Override
                public boolean test(StackTraceElement e) {
                    return e.getClassName().endsWith("$K0");
                }
            }).count();
        }
    }

    static class ForEachElements implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            final long[] c = new long[1];
            s.forEach(new Consumer<StackWalker.StackFrame>() {
                @Override
                public void accept(StackWalker.StackFrame f) {
                    if (f.toStackTraceElement().getClassName().endsWith("$K0")) {
                        c[0]++;
                    }
                }
            });
            return c[0];
        }
    }

    static class Collect implements Function<Stream<StackWalker.StackFrame>, List<StackWalker.StackFrame>> {
        @Override
        public List<StackWalker.StackFrame> apply(Stream<StackWalker.StackFrame> s) {
            final List<StackWalker.StackFrame> out = new ArrayList<>();
            s.forEach(new Consumer<StackWalker.StackFrame>() {
                @Override
                public void accept(StackWalker.StackFrame f) {
                    out.add(f);
                }
            });
            return out;
        }
    }

    static final Frames FRAMES = new Frames();
    static final Elements ELEMENTS = new Elements();
    static final ForEachElements FOREACH = new ForEachElements();
    static final Collect COLLECT = new Collect();

    static class K0 { static long go(int row, int n) { return K1.go(row, n); } }
    static class K1 { static long go(int row, int n) { return K2.go(row, n); } }
    static class K2 { static long go(int row, int n) { return K3.go(row, n); } }
    static class K3 { static long go(int row, int n) { return K4.go(row, n); } }
    static class K4 { static long go(int row, int n) { return K5.go(row, n); } }
    static class K5 { static long go(int row, int n) { return K6.go(row, n); } }
    static class K6 { static long go(int row, int n) { return K7.go(row, n); } }
    static class K7 { static long go(int row, int n) { return K8.go(row, n); } }
    static class K8 { static long go(int row, int n) { return K9.go(row, n); } }
    static class K9 { static long go(int row, int n) { return K10.go(row, n); } }
    static class K10 { static long go(int row, int n) { return K11.go(row, n); } }
    static class K11 { static long go(int row, int n) { return leaf(row, n); } }

    static List<StackWalker.StackFrame> kept;

    static long leaf(int row, int n) {
        long c = 0;
        switch (row) {
            case WALKER_FRAMES:
                for (int i = 0; i < n; i++) {
                    c += WALKER.walk(FRAMES);
                }
                return c;
            case WALKER_TO_STE:
                for (int i = 0; i < n; i++) {
                    c += WALKER.walk(ELEMENTS);
                }
                return c;
            case WALKER_FOREACH_STE:
                for (int i = 0; i < n; i++) {
                    c += WALKER.walk(FOREACH);
                }
                return c;
            default:
                if (kept == null) {
                    kept = WALKER.walk(COLLECT);
                }
                for (int i = 0; i < n; i++) {
                    for (StackWalker.StackFrame f : kept) {
                        if (f.toStackTraceElement().getClassName().endsWith("$K0")) {
                            c++;
                        }
                    }
                }
                return c;
        }
    }

    static void bench(String name, int row, int n) {
        long checksum = K0.go(row, n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long got = K0.go(row, n);
            long t1 = System.nanoTime();
            best = Math.min(best, t1 - t0);
            if (got != checksum) {
                System.out.println(name + " unstable " + got + " vs " + checksum);
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%s %.1f ns/op (min of %d)%n", name, (double) best / n, REPS);
    }

    public static void main(String[] args) {
        String only = args.length > 0 ? args[0] : null;
        String[] rows = {
            "walker-frames-12", "walker-to-ste-12", "walker-foreach-ste-12", "walker-ste-again-12",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            bench(rows[row], row, ITERS_WALK);
        }
    }
}
