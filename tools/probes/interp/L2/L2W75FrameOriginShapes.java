// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L2 -- the StackTraceElements the VM fills
// must not change when CRATONVM_STACK_TRACE_ORIGIN_RECORD=1 copies a class's
// origin fields (moduleName, moduleVersion, classLoaderName, format, the
// interned class name) from a per-VM record seeded by an EARLIER array. Each
// scenario reads its trace twice from the same call site: the first read seeds
// the record, the second is filled from it, and every getter of every element
// of the two arrays must agree ("same=true"). Covered fills:
// Throwable.getStackTrace() through four application classes, through a
// java.base frame (ArrayList.forEach calling a lambda body),
// Thread.getStackTrace(), and StackWalker's StackFrame.toStackTraceElement().
// Only the application and ArrayList frames are printed (a hidden lambda
// proxy is never printed), with ":NN)" line numbers stripped.
// Proposal: docs/internal/fixed-bugs/interpreter-i74-L2-proposal-per-vm-frame-origin-record-20261107-DONE-20261108.md.
//
// Run each of (the stdout must be the block below, byte for byte):
//   $B --java-home $J/.. -cp out L2W75FrameOriginShapes
//   env CRATONVM_STACK_TRACE_ORIGIN_RECORD=1 $B --java-home $J/.. -cp out L2W75FrameOriginShapes
//   $B --compatible --java-home $J/.. -cp out L2W75FrameOriginShapes
//   env CRATONVM_STACK_TRACE_ORIGIN_RECORD=1 $B --compatible --java-home $J/.. -cp out L2W75FrameOriginShapes
//   $J/java -cp out L2W75FrameOriginShapes
// A "same=false" line prints both arrays' differing element on stderr.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// chain-0 L2W75FrameOriginShapes$C|go|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes$C.go(L2W75FrameOriginShapes.java)
// chain-1 L2W75FrameOriginShapes$B|go|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes$B.go(L2W75FrameOriginShapes.java)
// chain-2 L2W75FrameOriginShapes$A|go|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes$A.go(L2W75FrameOriginShapes.java)
// chain-3 L2W75FrameOriginShapes|chain|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.chain(L2W75FrameOriginShapes.java)
// chain-4 L2W75FrameOriginShapes|take|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.take(L2W75FrameOriginShapes.java)
// chain-5 L2W75FrameOriginShapes|main|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.main(L2W75FrameOriginShapes.java)
// chain same=true
// java-base-0 L2W75FrameOriginShapes|lambda$javaBase$0|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.lambda$javaBase$0(L2W75FrameOriginShapes.java)
// java-base-1 java.util.ArrayList|forEach|ArrayList.java|null|java.base|java.base/java.util.ArrayList.forEach(ArrayList.java)
// java-base-2 L2W75FrameOriginShapes|javaBase|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.javaBase(L2W75FrameOriginShapes.java)
// java-base-3 L2W75FrameOriginShapes|take|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.take(L2W75FrameOriginShapes.java)
// java-base-4 L2W75FrameOriginShapes|main|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.main(L2W75FrameOriginShapes.java)
// java-base same=true
// thread-0 L2W75FrameOriginShapes$T|go|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes$T.go(L2W75FrameOriginShapes.java)
// thread-1 L2W75FrameOriginShapes|thread|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.thread(L2W75FrameOriginShapes.java)
// thread-2 L2W75FrameOriginShapes|take|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.take(L2W75FrameOriginShapes.java)
// thread-3 L2W75FrameOriginShapes|main|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.main(L2W75FrameOriginShapes.java)
// thread same=true
// walker-0 L2W75FrameOriginShapes$W|go|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes$W.go(L2W75FrameOriginShapes.java)
// walker-1 L2W75FrameOriginShapes|walker|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.walker(L2W75FrameOriginShapes.java)
// walker-2 L2W75FrameOriginShapes|take|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.take(L2W75FrameOriginShapes.java)
// walker-3 L2W75FrameOriginShapes|main|L2W75FrameOriginShapes.java|app|null|L2W75FrameOriginShapes.main(L2W75FrameOriginShapes.java)
// walker same=true
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import java.util.function.Function;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L2W75FrameOriginShapes {
    static class A {
        static StackTraceElement[] go() {
            return B.go();
        }
    }

    static class B {
        static StackTraceElement[] go() {
            return C.go();
        }
    }

    static class C {
        static StackTraceElement[] go() {
            return new Exception().getStackTrace();
        }
    }

    static class T {
        static StackTraceElement[] go() {
            return Thread.currentThread().getStackTrace();
        }
    }

    // A named Function, not a lambda: the probe's only lambda is javaBase's.
    static class W implements Function<Stream<StackWalker.StackFrame>, List<StackTraceElement>> {
        @Override
        public List<StackTraceElement> apply(Stream<StackWalker.StackFrame> s) {
            return s.map(StackWalker.StackFrame::toStackTraceElement).collect(Collectors.toList());
        }

        static StackTraceElement[] go() {
            return StackWalker.getInstance().walk(new W()).toArray(new StackTraceElement[0]);
        }
    }

    static StackTraceElement[] chain() {
        return A.go();
    }

    static StackTraceElement[] javaBase() {
        StackTraceElement[][] holder = new StackTraceElement[1][];
        List<Integer> one = new ArrayList<>(List.of(1));
        one.forEach(x -> holder[0] = new Exception().getStackTrace());
        return holder[0];
    }

    static StackTraceElement[] thread() {
        return T.go();
    }

    static StackTraceElement[] walker() {
        return W.go();
    }

    static StackTraceElement[] take(int which) {
        switch (which) {
            case 0:
                return chain();
            case 1:
                return javaBase();
            case 2:
                return thread();
            default:
                return walker();
        }
    }

    static boolean shown(StackTraceElement e) {
        String c = e.getClassName();
        return (c.startsWith("L2W75FrameOriginShapes") && !c.contains("$$Lambda"))
            || c.equals("java.util.ArrayList");
    }

    static String describe(StackTraceElement e) {
        return e.getClassName() + "|" + e.getMethodName() + "|" + e.getFileName() + "|"
            + e.getClassLoaderName() + "|" + e.getModuleName() + "|"
            + e.toString().replaceAll(":[0-9]+\\)", ")");
    }

    static String full(StackTraceElement e) {
        return describe(e) + "|" + e.getModuleVersion() + "|" + e.getLineNumber() + "|"
            + e.toString();
    }

    static void report(String name, StackTraceElement[] first, StackTraceElement[] second) {
        int shownIndex = 0;
        for (StackTraceElement e : first) {
            if (shown(e)) {
                System.out.println(name + "-" + shownIndex + " " + describe(e));
                shownIndex++;
            }
        }
        boolean same = first.length == second.length;
        if (!same) {
            System.err.println(name + " lengths " + first.length + " vs " + second.length);
        }
        for (int k = 0; same && k < first.length; k++) {
            same = full(first[k]).equals(full(second[k])) && first[k].equals(second[k]);
            if (!same) {
                System.err.println(name + " [" + k + "] " + full(first[k]));
                System.err.println(name + " [" + k + "] " + full(second[k]));
            }
        }
        System.out.println(name + " same=" + same);
    }

    public static void main(String[] args) {
        String[] names = {"chain", "java-base", "thread", "walker"};
        for (int which = 0; which < names.length; which++) {
            StackTraceElement[][] reads = new StackTraceElement[2][];
            for (int k = 0; k < 2; k++) {
                reads[k] = take(which);
            }
            report(names[which], reads[0], reads[1]);
        }
    }
}
