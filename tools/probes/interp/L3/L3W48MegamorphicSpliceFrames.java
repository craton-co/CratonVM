// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L3 -- a call a compiled body makes from
// INSIDE a spliced (inlined) method through the megamorphic stub keeps the
// spliced level in every trace and walk through it.
//
// `hot` runs `drive(op, x)` 40,000 times; `drive` is two bytecodes, so the
// optimizing tier splices it into `hot`, and its `op.apply(x)` becomes an
// inline cache. The first phase warms it with one receiver class (`A`); the
// later phases call it with other classes, which miss the cache's entries
// and reach the callee through the shared hashed stub. Each phase reports
// the last throw's frames and the last walk's frames, from the callee up to
// `hot`. HotSpot lists `drive` in every row.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// A throw: P$A.apply < P.drive < P.hot
// A walk: P$A.apply < P.drive < P.hot
// B throw: P$B.apply < P.drive < P.hot
// B walk: P$B.apply < P.drive < P.hot
// C throw: P$C.apply < P.drive < P.hot
// C walk: P$C.apply < P.drive < P.hot
// D throw: P$D.apply < P.drive < P.hot
// D walk: P$D.apply < P.drive < P.hot
// E throw: P$E.apply < P.drive < P.hot
// E walk: P$E.apply < P.drive < P.hot
// F throw: P$F.apply < P.drive < P.hot
// F walk: P$F.apply < P.drive < P.hot
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 48 (JIT on, local run of the wave-47 head): the
// rows of a receiver that reached the stub printed `P$X.apply < P.hot`; the
// hashed stub's `CALL R11` filed no inline-frame row
// (`L6/L6W37HiddenFramesHot.java`'s header has the trace). Positive control:
// `CRATONVM_DBG_SWCHAIN=1` prints `hot`'s activation with `chain=1` at the
// stub's return offset; `CRATONVM_JIT_IR_STUB_CALL_INLINE_ROWS=0` brings the
// old rows back. `--nojit` matches either way.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W48MegamorphicSpliceFrames`.
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Collectors;

public class L3W48MegamorphicSpliceFrames {
    interface Op {
        int apply(int x);
    }

    static boolean walkMode;
    static String walked;

    static int report(int x) {
        if (x < 0) {
            if (walkMode) {
                walked = StackWalker.getInstance().walk(s -> names(s.map(
                        f -> new String[] {f.getClassName(), f.getMethodName()})
                        .collect(Collectors.toList())));
                return 0;
            }
            throw new IllegalStateException("x");
        }
        return x;
    }

    static final class A implements Op {
        public int apply(int x) {
            return report(x) + 1;
        }
    }

    static final class B implements Op {
        public int apply(int x) {
            return report(x) + 2;
        }
    }

    static final class C implements Op {
        public int apply(int x) {
            return report(x) + 3;
        }
    }

    static final class D implements Op {
        public int apply(int x) {
            return report(x) + 4;
        }
    }

    static final class E implements Op {
        public int apply(int x) {
            return report(x) + 5;
        }
    }

    static final class F implements Op {
        public int apply(int x) {
            return report(x) + 6;
        }
    }

    static String shorten(String cls) {
        return cls.replace("L3W48MegamorphicSpliceFrames", "P");
    }

    static String names(List<String[]> frames) {
        List<String> out = new ArrayList<>();
        boolean started = false;
        for (String[] f : frames) {
            if (!started) {
                started = f[1].equals("apply");
                if (!started) {
                    continue;
                }
            }
            out.add(shorten(f[0]) + "." + f[1]);
            if (f[1].equals("hot")) {
                break;
            }
        }
        return String.join(" < ", out);
    }

    static int drive(Op op, int x) {
        return op.apply(x);
    }

    static String hot(Op op, boolean walk) {
        walkMode = walk;
        String last = "none";
        for (int i = 0; i < 40_000; i++) {
            boolean odd = (i & 63) == 63;
            try {
                drive(op, odd ? -1 : i);
                if (odd && walk) {
                    last = walked;
                }
            } catch (IllegalStateException e) {
                List<String[]> frames = new ArrayList<>();
                for (StackTraceElement el : e.getStackTrace()) {
                    frames.add(new String[] {el.getClassName(), el.getMethodName()});
                }
                last = names(frames);
            }
        }
        return last;
    }

    public static void main(String[] args) {
        Op[] ops = {new A(), new B(), new C(), new D(), new E(), new F()};
        String[] tags = {"A", "B", "C", "D", "E", "F"};
        for (int k = 0; k < ops.length; k++) {
            System.out.println(tags[k] + " throw: " + hot(ops[k], false));
            System.out.println(tags[k] + " walk: " + hot(ops[k], true));
        }
    }
}
