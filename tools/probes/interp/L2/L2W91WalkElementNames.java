// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 91, lane L2 -- fidelity guard for
// CRATONVM_STACK_TRACE_WALK_ELEMENT_INTERNED_NAMES (default OFF; page
// docs/internal/fixed-bugs/interpreter-i77-L2-proposal-walk-element-keeps-interned-names-KEPT-OFF-20261019.md).
// With the switch on, a walked frame's first toStackTraceElement() stores the
// frame's own interned `name` String as the element's methodName instead of
// decoding it and interning the text again. HotSpot interns both
// (java_lang_StackTraceElement::fill_in, StackFrameInfo's name), so the two
// are the SAME String object, and the element is memoised per frame.
//
// Rows (each walk is taken three times; the third round is printed and every
// round must agree, which exercises the per-VM frame-origin record path on
// rounds 2-3 and the slow fill on round 1):
//   <walk> frames=  number of frames walked (limit 4 / 3)
//   sameMethod      element.getMethodName() == frame.getMethodName() (identity)
//   equalMethod     the same by equals
//   equalClass      element.getClassName().equals(frame.getClassName())
//   equalLine       element.getLineNumber() == frame.getLineNumber()
//   memo            frame.toStackTraceElement() twice is the same element
//   names=          class.method of each frame, innermost first
//   stable          every round printed the same line
// Walks: plain (no options), retain (RETAIN_CLASS_REFERENCE), lambda (the
// walk taken inside a lambda body run from main; the lambda's method name is
// printed as its `lambda$main$` prefix only).
//
// Run (stdout must be the block, byte for byte, in every arm):
//   $J/javac -d out tools/probes/interp/L2/L2W91WalkElementNames.java
//   $B --java-home $J/.. -cp out L2W91WalkElementNames
//   env CRATONVM_STACK_TRACE_WALK_ELEMENT_INTERNED_NAMES=1 $B --java-home $J/.. -cp out L2W91WalkElementNames
//   the same two with --compatible, and with --nojit
//
// HOTSPOT_EXPECTED_BEGIN
// plain frames=4 sameMethod=4 equalMethod=4 equalClass=4 equalLine=4 memo=4
// plain names=L2W91WalkElementNames.check,L2W91WalkElementNames$A.again,L2W91WalkElementNames$B.run,L2W91WalkElementNames$A.run
// plain stable=true
// retain frames=4 sameMethod=4 equalMethod=4 equalClass=4 equalLine=4 memo=4
// retain names=L2W91WalkElementNames.check,L2W91WalkElementNames$A.again,L2W91WalkElementNames$B.run,L2W91WalkElementNames$A.run
// retain stable=true
// lambda frames=3 sameMethod=3 equalMethod=3 equalClass=3 equalLine=3 memo=3
// lambda names=L2W91WalkElementNames.check,L2W91WalkElementNames.lambda$main$,L2W91WalkElementNames.main
// lambda stable=true
// HOTSPOT_EXPECTED_END
import java.util.List;
import java.util.stream.Collectors;

public class L2W91WalkElementNames {
    static final StackWalker PLAIN = StackWalker.getInstance();
    static final StackWalker RETAIN =
            StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static String[] last = new String[2];

    static String check(StackWalker w, int depth) {
        List<StackWalker.StackFrame> frames =
                w.walk(s -> s.limit(depth).collect(Collectors.toList()));
        int same = 0;
        int eqMethod = 0;
        int eqClass = 0;
        int eqLine = 0;
        int memo = 0;
        StringBuilder names = new StringBuilder();
        for (StackWalker.StackFrame f : frames) {
            StackTraceElement e = f.toStackTraceElement();
            if (e.getMethodName() == f.getMethodName()) {
                same++;
            }
            if (e.getMethodName().equals(f.getMethodName())) {
                eqMethod++;
            }
            if (e.getClassName().equals(f.getClassName())) {
                eqClass++;
            }
            if (e.getLineNumber() == f.getLineNumber()) {
                eqLine++;
            }
            if (f.toStackTraceElement() == e) {
                memo++;
            }
            if (names.length() > 0) {
                names.append(',');
            }
            String m = e.getMethodName();
            if (m.startsWith("lambda$main$")) {
                m = "lambda$main$";
            }
            names.append(e.getClassName()).append('.').append(m);
        }
        return "frames=" + frames.size() + " sameMethod=" + same + " equalMethod=" + eqMethod
                + " equalClass=" + eqClass + " equalLine=" + eqLine + " memo=" + memo
                + "\n" + "names=" + names;
    }

    static final class A {
        static String run(StackWalker w) {
            return B.run(w);
        }

        static String again(StackWalker w) {
            return check(w, 4);
        }
    }

    static final class B {
        static String run(StackWalker w) {
            return A.again(w);
        }
    }

    static void report(String row, String[] rounds) {
        boolean stable = true;
        for (String r : rounds) {
            stable &= r.equals(rounds[rounds.length - 1]);
        }
        String[] parts = rounds[rounds.length - 1].split("\n");
        System.out.println(row + " " + parts[0]);
        System.out.println(row + " " + parts[1]);
        System.out.println(row + " stable=" + stable);
    }

    public static void main(String[] args) {
        String[] plain = new String[3];
        String[] retain = new String[3];
        String[] lambda = new String[3];
        Runnable body = () -> last[0] = check(PLAIN, 3);
        for (int i = 0; i < 3; i++) {
            plain[i] = A.run(PLAIN);
            retain[i] = A.run(RETAIN);
            body.run();
            lambda[i] = last[0];
        }
        report("plain", plain);
        report("retain", retain);
        report("lambda", lambda);
    }
}
