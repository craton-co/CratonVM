// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L2 -- what the two wave-77 switches must
// not change, and what the second must make equal to HotSpot:
//
//   CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC=1  (--jdk-only) the
//       StackTraceElement a walked frame's toStackTraceElement() answers
//       (StackTraceElement.of(StackFrameInfo) as a reviewed Intrinsic): rows
//       walker-* and java-base-*, compared with the Throwable's element of the
//       same frame (captured on the same source line, so every field agrees)
//       and memoised per frame.
//   CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY=1  (--compatible) the native
//       Throwable.getStackTrace() keeps the array it built and answers a copy,
//       the explicitly-set branch included: rows get-*, set-*, refill-*,
//       immutable-*, print-after-get-*.
//
// Proposals: docs/internal/fixed-bugs/interpreter-i76-L2-proposal-walker-element-of-stack-frame-info-20261109-DONE-20261110.md,
// docs/internal/fixed-bugs/interpreter-i76-L2-proposal-compatible-get-stack-trace-keeps-the-built-array-20261109-DONE-20261110.md
// (defect interpreter-i76-L2-compatible-get-stack-trace-returns-the-set-array-itself-20261109-FIXED-20261110.md).
// ":NN)" line numbers are stripped from every printed element.
//
// Run each of (the stdout must be the block below, byte for byte, except the
// lines named after it):
//   $B --java-home $J/.. -cp out L2W77StackTraceShapes
//   env CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC=1 $B --java-home $J/.. -cp out L2W77StackTraceShapes
//   env CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC=1 $B --nojit --java-home $J/.. -cp out L2W77StackTraceShapes
//   env CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY=1 $B --compatible --java-home $J/.. -cp out L2W77StackTraceShapes
//   $B --compatible --java-home $J/.. -cp out L2W77StackTraceShapes   (switch off: the known drift
//       "get-shared-elements false", "set-fresh-array false", "set-edit-isolated false")
//   $J/java -cp out L2W77StackTraceShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// walker-0 L2W77StackTraceShapes$K2|go|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes$K2.go(L2W77StackTraceShapes.java)
// walker-1 L2W77StackTraceShapes$K1|go|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes$K1.go(L2W77StackTraceShapes.java)
// walker-2 L2W77StackTraceShapes|walker|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes.walker(L2W77StackTraceShapes.java)
// walker-3 L2W77StackTraceShapes|main|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes.main(L2W77StackTraceShapes.java)
// walker agree=true memo=true
// java-base-0 L2W77StackTraceShapes$Grab|accept|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes$Grab.accept(L2W77StackTraceShapes.java)
// java-base-1 java.util.ArrayList|forEach|ArrayList.java|null|java.base|ver=true|java.base/java.util.ArrayList.forEach(ArrayList.java)
// java-base-2 L2W77StackTraceShapes|javaBase|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes.javaBase(L2W77StackTraceShapes.java)
// java-base-3 L2W77StackTraceShapes|main|L2W77StackTraceShapes.java|app|null|ver=false|L2W77StackTraceShapes.main(L2W77StackTraceShapes.java)
// java-base agree=true memo=true
// get-fresh-array true
// get-shared-elements true
// get-edit-isolated true
// set-fresh-array true
// set-edit-isolated true
// set-shared-elements true
// refill-top refill
// immutable-length 0 fresh=true
// print-after-get-0 java.lang.Exception: x
// print-after-get-1 <TAB>at L2W77StackTraceShapes.make(L2W77StackTraceShapes.java)
// print-after-get-2 <TAB>at L2W77StackTraceShapes.printAfterGet(L2W77StackTraceShapes.java)
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.stream.Stream;

public class L2W77StackTraceShapes {
    static final StackWalker WALKER = StackWalker.getInstance();
    static boolean memoBroken;

    // Every walked frame's element, asked twice (the JDK memoises it per frame).
    static class Collect implements Function<Stream<StackWalker.StackFrame>, List<StackTraceElement>> {
        @Override
        public List<StackTraceElement> apply(Stream<StackWalker.StackFrame> s) {
            List<StackTraceElement> out = new ArrayList<>();
            s.forEach(new Consumer<StackWalker.StackFrame>() {
                @Override
                public void accept(StackWalker.StackFrame f) {
                    StackTraceElement e = f.toStackTraceElement();
                    if (e != f.toStackTraceElement()) {
                        memoBroken = true;
                    }
                    out.add(e);
                }
            });
            return out;
        }
    }

    static final Collect COLLECT = new Collect();

    static List<StackTraceElement> walked;
    static StackTraceElement[] thrown;

    // The walk and the throwable on ONE source line: the frames they share
    // carry the same line number.
    static class K2 {
        static void go() {
            walked = WALKER.walk(COLLECT); thrown = new Exception().getStackTrace();
        }
    }

    static class K1 {
        static void go() {
            K2.go();
        }
    }

    // A named Consumer: its accept frame sits under ArrayList.forEach.
    static class Grab implements Consumer<Integer> {
        @Override
        public void accept(Integer x) {
            walked = WALKER.walk(COLLECT); thrown = new Exception().getStackTrace();
        }
    }

    static void walker() {
        K1.go();
    }

    static void javaBase() {
        new ArrayList<>(List.of(1)).forEach(new Grab());
    }

    static boolean shown(StackTraceElement e) {
        String c = e.getClassName();
        return (c.startsWith("L2W77StackTraceShapes") && !c.contains("$$Lambda"))
            || c.equals("java.util.ArrayList");
    }

    static String strip(String s) {
        return s.replaceAll(":[0-9]+\\)", ")");
    }

    static String describe(StackTraceElement e) {
        return e.getClassName() + "|" + e.getMethodName() + "|" + e.getFileName() + "|"
            + e.getClassLoaderName() + "|" + e.getModuleName() + "|ver="
            + (e.getModuleVersion() != null) + "|" + strip(e.toString());
    }

    static List<StackTraceElement> shownOf(List<StackTraceElement> all) {
        List<StackTraceElement> out = new ArrayList<>();
        for (StackTraceElement e : all) {
            if (shown(e)) {
                out.add(e);
            }
        }
        return out;
    }

    static void report(String name) {
        List<StackTraceElement> w = shownOf(walked);
        List<StackTraceElement> t = shownOf(List.of(thrown));
        for (int i = 0; i < w.size(); i++) {
            System.out.println(name + "-" + i + " " + describe(w.get(i)));
        }
        boolean agree = w.size() == t.size();
        for (int i = 0; agree && i < w.size(); i++) {
            StackTraceElement a = w.get(i);
            StackTraceElement b = t.get(i);
            if (!a.equals(b) || !a.toString().equals(b.toString())) {
                agree = false;
                System.err.println(name + " differs at " + i + ": walker " + a + " / throwable " + b);
            }
        }
        System.out.println(name + " agree=" + agree + " memo=" + !memoBroken);
    }

    static Throwable make() {
        return new Exception("x");
    }

    static void refill(Throwable t) {
        t.fillInStackTrace();
    }

    static class Immutable extends Exception {
        Immutable() {
            super("i", null, false, false);
        }
    }

    static void getAndSet() {
        Throwable t = make();
        StackTraceElement[] a = t.getStackTrace();
        StackTraceElement[] b = t.getStackTrace();
        System.out.println("get-fresh-array " + (a != b));
        System.out.println("get-shared-elements " + (a.length > 0 && a[0] == b[0]));
        a[0] = null;
        System.out.println("get-edit-isolated " + (t.getStackTrace()[0] != null));
        StackTraceElement[] set = {new StackTraceElement("p.C", "m", "C.java", 7)};
        t.setStackTrace(set);
        StackTraceElement[] c = t.getStackTrace();
        StackTraceElement[] d = t.getStackTrace();
        System.out.println("set-fresh-array " + (c != set && c != d));
        c[0] = null;
        System.out.println("set-edit-isolated " + (t.getStackTrace()[0] != null));
        System.out.println("set-shared-elements " + (t.getStackTrace()[0] == set[0]));
        Throwable u = make();
        u.getStackTrace();
        refill(u);
        System.out.println("refill-top " + u.getStackTrace()[0].getMethodName());
        Throwable imm = new Immutable();
        System.out.println("immutable-length " + imm.getStackTrace().length
            + " fresh=" + (imm.getStackTrace() != imm.getStackTrace()));
    }

    static void printAfterGet() {
        Throwable t = make();
        t.getStackTrace();
        StringWriter sw = new StringWriter();
        t.printStackTrace(new PrintWriter(sw, true));
        String[] lines = sw.toString().split("\n");
        for (int i = 0; i < 3 && i < lines.length; i++) {
            System.out.println("print-after-get-" + i + " " + strip(lines[i].replace("\r", "").replace("\t", "<TAB>")));
        }
    }

    public static void main(String[] args) {
        walker();
        report("walker");
        javaBase();
        report("java-base");
        getAndSet();
        printAfterGet();
    }
}
