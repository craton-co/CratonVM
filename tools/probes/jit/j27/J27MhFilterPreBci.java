// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane GCMAP): while the PRE call of a compiled constant filter species
// runs (the filter of filterArguments, the target of filterReturnValue), the caller's
// frame must report the bci of its `invokeExact` (HotSpot 25 hides the LambdaForm
// frames between), not the bci of the `getstatic` that loads the handle. Page
// docs/known-issues/jit/j27-mhfilter-pre-call-frame-bci-is-the-getstatic-20261028.md
// Each caller runs 30000 times (tier-up), then once with -1 so the PRE callee walks
// the stack; the fact is the frame below look and the PRE callee: method, bci, line.
// HotSpot 25 (expected): fact argFilter argFilter bci=4 line=51 ret=0 sum=30531120
//                        fact retFilter retFilter bci=4 line=56 ret=0 sum=30531120
//                        DONE
//   java J27MhFilterPreBci
// javap -c: argFilter / retFilter are `getstatic` 0, `iload_0` 3, `invokevirtual` 4.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class J27MhFilterPreBci {
    static String seen = "NONE";

    static void look(int x) {
        if (x >= 0) return;
        seen = StackWalker.getInstance()
            .walk(s -> s.skip(2).findFirst())
            .map(f -> f.getMethodName() + " bci=" + f.getByteCodeIndex() + " line=" + f.getLineNumber())
            .orElse("EMPTY");
    }

    static int twice(int x) { return 2 * x; }

    static int check(int x) { look(x); return x + 1; }

    static int inc(int x) { look(x); return x + 1; }

    static final MethodHandle FA;
    static final MethodHandle FR;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodType ii = MethodType.methodType(int.class, int.class);
            MethodHandle twice = l.findStatic(J27MhFilterPreBci.class, "twice", ii);
            FA = MethodHandles.filterArguments(twice, 0, l.findStatic(J27MhFilterPreBci.class, "check", ii));
            FR = MethodHandles.filterReturnValue(l.findStatic(J27MhFilterPreBci.class, "inc", ii), twice);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // Line 51: the filter `check` is the PRE call.
    static int argFilter(int x) throws Throwable {
        return (int) FA.invokeExact(x);
    }

    // Line 56: the target `inc` is the PRE call.
    static int retFilter(int x) throws Throwable {
        return (int) FR.invokeExact(x);
    }

    interface Call { int run(int x) throws Throwable; }

    static void fact(String name, Call c) {
        long sum = 0;
        try {
            for (int i = 0; i < 30000; i++) sum += c.run(i & 1023);
            seen = "NONE";
            int last = c.run(-1);
            System.out.println("fact " + name + " " + seen + " ret=" + last + " sum=" + sum);
        } catch (Throwable t) {
            System.out.println("fact " + name + " THREW_" + t);
        }
    }

    public static void main(String[] args) {
        fact("argFilter", J27MhFilterPreBci::argFilter);
        fact("retFilter", J27MhFilterPreBci::retFilter);
        System.out.println("DONE");
    }
}
