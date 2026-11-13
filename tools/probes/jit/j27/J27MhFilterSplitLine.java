// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 1 (lane MHINDY): a constant filterArguments species whose filter throws
// must report, for the caller's frame, the line HotSpot 25 reports -- the line of the
// `invokeExact`, not of the `getstatic` that loads the handle -- also once the caller
// is compiled. Page
// docs/known-issues/jit/jr21-mhfilter-pre-call-frame-reports-the-getstatic-line-20261014.md
// Each caller runs 30000 times with a non-negative argument (tier-up), then once with
// -1 so the filter throws; the fact is the caller frame's method and line.
//   java J27MhFilterSplitLine
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class J27MhFilterSplitLine {
    static int twice(int x) { return 2 * x; }

    static int check(int x) {
        if (x < 0) throw new IllegalArgumentException("neg " + x);
        return x + 1;
    }

    static final MethodHandle FT;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodType ii = MethodType.methodType(int.class, int.class);
            MethodHandle target = l.findStatic(J27MhFilterSplitLine.class, "twice", ii);
            MethodHandle filter = l.findStatic(J27MhFilterSplitLine.class, "check", ii);
            FT = MethodHandles.filterArguments(target, 0, filter);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // The getstatic on line 37, the invokeExact on line 38 (HotSpot reports 38).
    static int split(int x) throws Throwable {
        return (int) FT
            .invokeExact(x);
    }

    // One line (line 43): both pcs name it.
    static int oneLine(int x) throws Throwable {
        return (int) FT.invokeExact(x);
    }

    interface Call { int run(int x) throws Throwable; }

    static void fact(String name, Call c) {
        long sum = 0;
        try {
            for (int i = 0; i < 30000; i++) sum += c.run(i & 1023);
        } catch (Throwable t) {
            System.out.println("fact " + name + " WARMUP_THREW_" + t);
            return;
        }
        String out;
        try {
            out = "NO_THROW_" + c.run(-1);
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StackTraceElement callee = st.length > 0 ? st[0] : null;
            StackTraceElement caller = st.length > 1 ? st[1] : null;
            out = t.getClass().getSimpleName()
                + "/" + (callee == null ? "?" : callee.getMethodName())
                + "/" + (caller == null ? "?" : caller.getMethodName() + ":" + caller.getLineNumber());
        }
        System.out.println("fact " + name + " " + out + " sum=" + sum);
    }

    public static void main(String[] args) {
        fact("split", J27MhFilterSplitLine::split);
        fact("oneLine", J27MhFilterSplitLine::oneLine);
        System.out.println("DONE");
    }
}
