// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L2 -- StackTraceElement.toString() of
// every element shape the JDK body distinguishes, which
// CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC=1 (toString() rendered in
// Rust under --jdk-only) must not move: elements built by the public
// constructors (format 0: the loader name and the module version print),
// a named loader of an unnamed module ("ldr//"), empty names, Native Method,
// a file without a line, no file, line 0, non-ASCII and a lone surrogate
// (compared, not printed: stdout's encoding varies), a fresh String per call,
// and captured frames (app, java.base, Thread.getStackTrace, a cause chain
// printed by printStackTrace). Captured line numbers are stripped
// (":NN)" -> ")") so the block does not depend on the JDK's sources.
// Captured frames' class names are also covered by L2W74StackTraceElementShapes
// and L2W75FrameOriginShapes; CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT=1 must
// keep those two and this one SAME.
// Proposal: docs/internal/fixed-bugs/interpreter-i75-L2-proposal-jdk-only-nojit-print-stack-trace-runs-the-text-path-in-java-20261108-DONE-20261109.md.
//
// Run each of (the stdout must be the block below, byte for byte):
//   $B --java-home $J/.. -cp out L2W76StackTraceTextShapes
//   env CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC=1 $B --java-home $J/.. -cp out L2W76StackTraceTextShapes
//   env CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT=1 $B --java-home $J/.. -cp out L2W76StackTraceTextShapes
//   (each also with --nojit, and with --compatible, where the toString switch registers nothing)
//   $J/java -cp out L2W76StackTraceTextShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// four-arg p.C.m(C.java:3)
// native p.C.m(Native Method)
// no-line p.C.m(C.java)
// no-file p.C.m(Unknown Source)
// no-file-native p.C.m(Native Method)
// line-zero p.C.m(C.java:0)
// seven-arg app/mod@1.0/p.C.m(C.java:3)
// loader-only ldr//p.C.m(C.java:3)
// module-only mod/p.C.m(C.java:3)
// module-version mod@2/p.C.m(C.java:3)
// version-only p.C.m(C.java:3)
// empty-names p.C.m(C.java:3)
// empty-file p.C.m(:3)
// empty-method p.C.(C.java:3)
// java-base-ctor java.base@25/java.lang.Thread.run(Thread.java:1)
// non-ascii true
// lone-surrogate true
// fresh true equal true
// app L2W76StackTraceTextShapes.main(L2W76StackTraceTextShapes.java)
// java-base java.base/java.util.ArrayList.forEach(ArrayList.java)
// thread-top java.base/java.lang.Thread.getStackTrace(Thread.java)
// printed java.lang.IllegalStateException: outer|at L2W76StackTraceTextShapes.main(L2W76StackTraceTextShapes.java)|Caused by: java.lang.Exception: inner|at L2W76StackTraceTextShapes.inner(L2W76StackTraceTextShapes.java)|... 1 more|
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;

public class L2W76StackTraceTextShapes {
    static String strip(String s) {
        return s.replaceAll(":[0-9]+\\)", ")");
    }

    static void show(String label, StackTraceElement e) {
        System.out.println(label + " " + e.toString());
    }

    static StackTraceElement seven(String loader, String module, String version, String file, int line) {
        return new StackTraceElement(loader, module, version, "p.C", "m", file, line);
    }

    // A named Consumer (no lambda, so no hidden proxy frame): its accept frame
    // sits under ArrayList.forEach.
    static class Grab implements Consumer<Integer> {
        StackTraceElement seen;

        @Override
        public void accept(Integer x) {
            seen = new Exception().getStackTrace()[1];
        }
    }

    static Exception inner() {
        return new Exception("inner");
    }

    public static void main(String[] args) {
        show("four-arg", new StackTraceElement("p.C", "m", "C.java", 3));
        show("native", new StackTraceElement("p.C", "m", "C.java", -2));
        show("no-line", new StackTraceElement("p.C", "m", "C.java", -1));
        show("no-file", new StackTraceElement("p.C", "m", null, 7));
        show("no-file-native", new StackTraceElement("p.C", "m", null, -2));
        show("line-zero", new StackTraceElement("p.C", "m", "C.java", 0));
        show("seven-arg", seven("app", "mod", "1.0", "C.java", 3));
        show("loader-only", seven("ldr", null, null, "C.java", 3));
        show("module-only", seven(null, "mod", null, "C.java", 3));
        show("module-version", seven(null, "mod", "2", "C.java", 3));
        show("version-only", seven(null, null, "2", "C.java", 3));
        show("empty-names", seven("", "", "", "C.java", 3));
        show("empty-file", seven(null, null, null, "", 3));
        show("empty-method", new StackTraceElement("p.C", "", "C.java", 3));
        show("java-base-ctor", new StackTraceElement(null, "java.base", "25", "java.lang.Thread", "run", "Thread.java", 1));

        StackTraceElement wide = new StackTraceElement("p.Ü", "méthode", "F.java", 1);
        System.out.println("non-ascii " + wide.toString().equals("p.Ü.méthode(F.java:1)"));
        StackTraceElement lone = new StackTraceElement("A\ud800B", "m", null, -1);
        System.out.println("lone-surrogate " + lone.toString().equals("A\ud800B.m(Unknown Source)"));

        StackTraceElement four = new StackTraceElement("p.C", "m", "C.java", 3);
        String a = four.toString();
        String b = four.toString();
        System.out.println("fresh " + (a != b) + " equal " + a.equals(b));

        StackTraceElement[] here = new Exception().getStackTrace();
        System.out.println("app " + strip(here[0].toString()));

        Grab grab = new Grab();
        new ArrayList<>(List.of(1)).forEach(grab);
        System.out.println("java-base " + strip(grab.seen.toString()));

        StackTraceElement[] thread = Thread.currentThread().getStackTrace();
        System.out.println("thread-top " + strip(thread[0].toString()));

        StringWriter sw = new StringWriter();
        new IllegalStateException("outer", inner()).printStackTrace(new PrintWriter(sw, true));
        StringBuilder printed = new StringBuilder();
        for (String line : sw.toString().split("\\R")) {
            printed.append(strip(line.trim())).append('|');
        }
        System.out.println("printed " + printed);
    }
}
