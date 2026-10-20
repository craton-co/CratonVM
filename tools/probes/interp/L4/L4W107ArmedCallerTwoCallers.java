// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L4: behaviour probe for
// `CRATONVM_MH_ARMED_CALLER` (default off; page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`,
// "Wave 107").
//
// With the switch on, a compiled `invokeExact` site publishes its own class
// with the call-site descriptor, and the before-linker lanes (field, direct)
// probe their per-thread site memo under that class before walking the stack
// for the caller. This probe calls the SAME handles at the SAME site text from
// several callers -- two classes, a lambda body, a site the JIT may inline
// into another method, two handles alternating at one site -- after each is
// warm (30000 calls), and checks every answer, a null base and a wrong-typed
// handle after warm calls. Every line must be the same with the switch on and
// off, both modes, JIT and --nojit.
//
// Run: javac -d . L4W107ArmedCallerTwoCallers.java
//      java -cp . L4W107ArmedCallerTwoCallers                                  (HotSpot)
//      cratonvm --jdk-only -cp . L4W107ArmedCallerTwoCallers
//      CRATONVM_MH_ARMED_CALLER=1 cratonvm --jdk-only -cp . L4W107ArmedCallerTwoCallers
//      CRATONVM_DBG=mh-finder-census CRATONVM_MH_ARMED_CALLER=1 cratonvm --jdk-only -cp . \
//          L4W107ArmedCallerTwoCallers 2> c.log; grep armed-caller c.log
//          (expect armed-caller.mismatch absent or 0)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// callerA-get 90000
// callerB-get 90000
// alternating-callers 90000
// lambda-get 90000
// inlined-get 90000
// two-handles-one-site 120000
// virtual 360000
// static 240000
// null-base java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// setter-two-callers 29999
// after-setter 90000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W107ArmedCallerTwoCallers {
    public static class Box {
        public static int COUNT = 4;
        public int v = 3;
        public int w = 5;
        public long l = 9L;

        public int twice() {
            return v * 2;
        }
    }

    static final MethodHandle GET;
    static final MethodHandle GETW;
    static final MethodHandle GETL;
    static final MethodHandle SET;
    static final MethodHandle SGET;
    static final MethodHandle VIRT;
    /** A non-constant handle, so one site sees more than one handle. */
    static MethodHandle cur;

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            GET = lk.findGetter(Box.class, "v", int.class);
            GETW = lk.findGetter(Box.class, "w", int.class);
            GETL = lk.findGetter(Box.class, "l", long.class);
            SET = lk.findSetter(Box.class, "v", int.class);
            SGET = lk.findStaticGetter(Box.class, "COUNT", int.class);
            VIRT = lk.findVirtual(Box.class, "twice", MethodType.methodType(int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final class CallerA {
        static int get(Box b) throws Throwable {
            return (int) GET.invokeExact(b);
        }

        static int getCur(Box b) throws Throwable {
            return (int) cur.invokeExact(b);
        }

        static void set(Box b, int x) throws Throwable {
            SET.invokeExact(b, x);
        }

        static int twice(Box b) throws Throwable {
            return (int) VIRT.invokeExact(b);
        }

        static int count() throws Throwable {
            return (int) SGET.invokeExact();
        }
    }

    static final class CallerB {
        static int get(Box b) throws Throwable {
            return (int) GET.invokeExact(b);
        }

        static void set(Box b, int x) throws Throwable {
            SET.invokeExact(b, x);
        }

        static int twice(Box b) throws Throwable {
            return (int) VIRT.invokeExact(b);
        }

        static int count() throws Throwable {
            return (int) SGET.invokeExact();
        }
    }

    interface Getter {
        int get(Box b) throws Throwable;
    }

    /** CallerA's site, small enough for the JIT to inline here. */
    static int viaA(Box b) throws Throwable {
        return CallerA.get(b);
    }

    public static void main(String[] args) throws Throwable {
        final int n = 30000;
        Box b = new Box();

        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += CallerA.get(b);
        }
        System.out.println("callerA-get " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += CallerB.get(b);
        }
        System.out.println("callerB-get " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (i % 2 == 0) ? CallerA.get(b) : CallerB.get(b);
        }
        System.out.println("alternating-callers " + sum);

        Getter lambda = x -> (int) GET.invokeExact(x);
        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += lambda.get(b);
        }
        System.out.println("lambda-get " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += viaA(b);
        }
        System.out.println("inlined-get " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            cur = (i % 2 == 0) ? GET : GETW;
            sum += CallerA.getCur(b);
        }
        System.out.println("two-handles-one-site " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += CallerA.twice(b) + CallerB.twice(b);
        }
        System.out.println("virtual " + sum);

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += CallerA.count() + CallerB.count();
        }
        System.out.println("static " + sum);

        try {
            CallerB.get(null);
            System.out.println("null-base no exception");
        } catch (Throwable t) {
            System.out.println("null-base " + t.getClass().getName());
        }

        cur = GETL;
        try {
            CallerA.getCur(b);
            System.out.println("wrong-type no exception");
        } catch (Throwable t) {
            System.out.println("wrong-type " + t.getClass().getName());
        }
        cur = GET;

        for (int i = 0; i < n; i++) {
            if (i % 2 == 0) {
                CallerA.set(b, i);
            } else {
                CallerB.set(b, i);
            }
        }
        System.out.println("setter-two-callers " + b.v);
        b.v = 3;

        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (i % 2 == 0) ? CallerB.get(b) : viaA(b);
        }
        System.out.println("after-setter " + sum);
    }
}
