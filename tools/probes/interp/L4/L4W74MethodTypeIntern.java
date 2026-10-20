// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L4 -- one default-off switch:
//   CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=1 (proposal
//     `interpreter-i74-L4-proposal-the-jdk-only-finder-mint-cost-model-DONE-20261014`): a
//     reviewed `Intrinsic` over `MethodType.makeImpl` (registered only while
//     the switch is on) answers a type this thread interned before, in the
//     same collection epoch, from a per-thread memo keyed by the return and
//     parameter `Class` objects; every other call runs the JDK's own body.
//
// What must not move: `MethodType` identity (the intern table's one object
// per equal type, across every factory, a finder's handle type, a collection
// and two threads), the defensive copy of a caller's array, the order of the
// parameters, and the exceptions of a bad type (a `null` component, a `void`
// parameter): their class, message and the one `makeImpl` frame of their
// stack trace. Every row runs three times (the first fills the memo, the
// others are served by it) and prints `<answer> x3` when the three agree.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// same-0: true x3
// same-1: true x3
// insert: true x3
// array-form: true x3
// list-form: true x3
// descriptor: true x3
// finder-type: true x3
// generic: true x3
// order: false x3
// order-strings: (int,long)void (long,int)void x3
// to-string: (Box)int x3
// defensive-copy: Box true x3
// many-params: true x3
// after-gc: true x3
// threads: true
// npe-ptype: java.lang.NullPointerException makeImpl=1 caller=methodType x3
// npe-rtype: java.lang.NullPointerException makeImpl=1 caller=methodType x3
// iae-void: java.lang.IllegalArgumentException: parameter type cannot be void x3
// found-after-iae: true x3
// HOTSPOT_EXPECTED_END
//
// Switch sets, `--compatible` and `--jdk-only` (each with `--nojit` too):
//   A: (none)
//   B: CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=1
// Every row must be SAME in both sets, in both modes.
//
// SETUP: none; `java|cratonvm [--compatible|--jdk-only] [--nojit] -cp . L4W74MethodTypeIntern`.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W74MethodTypeIntern {
    public static class Box {
        public int v = 3;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String once(Row r) {
        try {
            Object o = r.run();
            return String.valueOf(o);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String describe(Throwable t) {
        if (t instanceof NullPointerException) {
            int makeImpl = 0;
            String caller = "?";
            StackTraceElement[] st = t.getStackTrace();
            for (int i = 0; i < st.length; i++) {
                if (st[i].getClassName().equals("java.lang.invoke.MethodType")
                        && st[i].getMethodName().equals("makeImpl")) {
                    makeImpl++;
                    if (i + 1 < st.length) caller = st[i + 1].getMethodName();
                }
            }
            return t.getClass().getName() + " makeImpl=" + makeImpl + " caller=" + caller;
        }
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void row(String name, Row r) {
        List<String> answers = new ArrayList<>();
        for (int i = 0; i < 3; i++) answers.add(once(r));
        boolean same = answers.get(0).equals(answers.get(1)) && answers.get(1).equals(answers.get(2));
        System.out.println(name + ": " + (same ? answers.get(0) + " x3" : String.join(" | ", answers)));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        ClassLoader loader = L4W74MethodTypeIntern.class.getClassLoader();

        row("same-0", () -> MethodType.methodType(int.class) == MethodType.methodType(int.class));
        row("same-1", () -> MethodType.methodType(int.class, Box.class)
                == MethodType.methodType(int.class, Box.class));
        row("insert", () -> MethodType.methodType(int.class).insertParameterTypes(0, Box.class)
                == MethodType.methodType(int.class, Box.class));
        row("array-form", () -> MethodType.methodType(int.class, new Class<?>[] {Box.class})
                == MethodType.methodType(int.class, Box.class));
        row("list-form", () -> MethodType.methodType(int.class, List.<Class<?>>of(Box.class))
                == MethodType.methodType(int.class, Box.class));
        row("descriptor", () -> MethodType.fromMethodDescriptorString("(LL4W74MethodTypeIntern$Box;)I", loader)
                == MethodType.methodType(int.class, Box.class));
        row("finder-type", () -> lk.findGetter(Box.class, "v", int.class).type()
                == MethodType.methodType(int.class, Box.class));
        row("generic", () -> MethodType.methodType(Object.class) == MethodType.genericMethodType(0));
        row("order", () -> MethodType.methodType(void.class, int.class, long.class)
                == MethodType.methodType(void.class, long.class, int.class));
        row("order-strings", () -> MethodType.methodType(void.class, int.class, long.class) + " "
                + MethodType.methodType(void.class, long.class, int.class));
        row("to-string", () -> MethodType.methodType(int.class, Box.class).toString());
        row("defensive-copy", () -> {
            Class<?>[] a = {Box.class};
            MethodType mt = MethodType.methodType(int.class, a);
            a[0] = String.class;
            return mt.parameterType(0).getSimpleName() + " "
                    + (MethodType.methodType(int.class, a) == MethodType.methodType(int.class, String.class));
        });
        row("many-params", () -> MethodType.methodType(int.class, int.class, long.class, Box.class, String.class,
                        Object.class, byte.class, char.class, double.class)
                == MethodType.methodType(int.class, int.class, long.class, Box.class, String.class,
                        Object.class, byte.class, char.class, double.class));
        row("after-gc", () -> {
            MethodType held = MethodType.methodType(int.class, Box.class, long.class);
            System.gc();
            return held == MethodType.methodType(int.class, Box.class, long.class);
        });
        MethodType[] seen = new MethodType[2];
        Thread[] ts = new Thread[2];
        for (int i = 0; i < 2; i++) {
            final int k = i;
            ts[i] = new Thread(() -> {
                MethodType last = null;
                for (int j = 0; j < 50; j++) last = MethodType.methodType(int.class, Box.class, String.class);
                seen[k] = last;
            });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("threads: " + (seen[0] == seen[1]
                && seen[0] == MethodType.methodType(int.class, Box.class, String.class)));
        row("npe-ptype", () -> MethodType.methodType(int.class, (Class<?>) null));
        row("npe-rtype", () -> MethodType.methodType((Class<?>) null, int.class));
        row("iae-void", () -> MethodType.methodType(int.class, void.class));
        row("found-after-iae", () -> MethodType.methodType(int.class, Box.class)
                == MethodType.methodType(int.class, new Class<?>[] {Box.class}));
    }
}
