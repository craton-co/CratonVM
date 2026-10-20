// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L3 -- an inherited static called through
// a subclass name initializes the DECLARING class only (JVMS 5.5): the
// subclass, whose <clinit> logs, is never initialized by any of the routes,
// and is initialized later by a call to a static it declares itself.
// CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED (proposal
// i71-L3-proposal-an-inherited-static-owner-answers-from-its-superclasses-bits)
// changes how the by-name routes answer "nothing to initialize" for such an
// owner; the output must not move.
//
// Routes (each its own Base/Sub pair, so each starts with both uninitialized;
// N calls each so the steady state and, by default, compiled code run too):
//   direct    `SubN.helper(i)` (invokestatic through the subclass)
//   reflect   Sub.class.getMethod("helper") -> Method.invoke
//   handle    lookup().findStatic(Sub.class, "helper")
//   lambda    the method reference `Sub::helper`
//   subs-later  a static each Sub declares, called afterwards: only now do
//               the four <clinit>s run
//
// Run --compatible and --jdk-only, default and --nojit, with
// CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED unset and =1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// direct before=[] after=[Base1;] sum=200030000
// reflect before=[] after=[Base2;] sum=200030000
// handle before=[] after=[Base3;] sum=200030000
// lambda after=[Base4;] sum=200030000
// subs-later [Sub1;Sub2;Sub3;Sub4;] sum=8
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.IntUnaryOperator;

public class L3W72InheritedStaticInit {
    static final int N = 20_000;
    static final StringBuffer LOG = new StringBuffer();

    static String take() {
        String s = "[" + LOG + "]";
        LOG.setLength(0);
        return s;
    }

    public static class Base1 {
        static { LOG.append("Base1;"); }
        public static int helper(int x) { return x + 1; }
    }
    public static class Sub1 extends Base1 {
        static { LOG.append("Sub1;"); }
        public static int own() { return 2; }
    }
    public static class Base2 {
        static { LOG.append("Base2;"); }
        public static int helper(int x) { return x + 1; }
    }
    public static class Sub2 extends Base2 {
        static { LOG.append("Sub2;"); }
        public static int own() { return 2; }
    }
    public static class Base3 {
        static { LOG.append("Base3;"); }
        public static int helper(int x) { return x + 1; }
    }
    public static class Sub3 extends Base3 {
        static { LOG.append("Sub3;"); }
        public static int own() { return 2; }
    }
    public static class Base4 {
        static { LOG.append("Base4;"); }
        public static int helper(int x) { return x + 1; }
    }
    public static class Sub4 extends Base4 {
        static { LOG.append("Sub4;"); }
        public static int own() { return 2; }
    }

    public static void main(String[] args) throws Throwable {
        // Each loop adds i + 1 for i in [1, N]: N(N+1)/2 + N = 200030000.
        String before = take();
        long s = 0;
        for (int i = 1; i <= N; i++) {
            s += Sub1.helper(i);
        }
        System.out.println("direct before=" + before + " after=" + take() + " sum=" + s);

        Method m = Sub2.class.getMethod("helper", int.class);
        before = take();
        s = 0;
        for (int i = 1; i <= N; i++) {
            s += (Integer) m.invoke(null, i);
        }
        System.out.println("reflect before=" + before + " after=" + take() + " sum=" + s);

        MethodHandle h = MethodHandles.lookup()
                .findStatic(Sub3.class, "helper", MethodType.methodType(int.class, int.class));
        before = take();
        s = 0;
        for (int i = 1; i <= N; i++) {
            s += (int) h.invokeExact(i);
        }
        System.out.println("handle before=" + before + " after=" + take() + " sum=" + s);

        IntUnaryOperator op = Sub4::helper;
        s = 0;
        for (int i = 1; i <= N; i++) {
            s += op.applyAsInt(i);
        }
        System.out.println("lambda after=" + take() + " sum=" + s);

        int own = Sub1.own() + Sub2.own() + Sub3.own() + Sub4.own();
        System.out.println("subs-later " + take() + " sum=" + own);
    }
}
