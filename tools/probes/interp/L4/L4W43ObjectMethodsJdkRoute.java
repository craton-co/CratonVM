// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4
// (`i42-L4-objectmethods-getters-the-native-linkage-still-hands-to-the-jdk`,
// rows 1-3): the getter shapes the `ObjectMethods` native linkage leaves
// "undecided" are linked by the JDK's own `ObjectMethods.bootstrap`, the route
// this probe calls DIRECTLY. Its `toString` needs `StringConcatFactory` ->
// `MethodHandle.viewAsType` -> `copyWith`, abstract on CratonVM's synthetic
// handles until wave 43 (see `L4W43CopyWithViews`).
//
//   protected-*   a PROTECTED field of a JDK class in another package, read
//                 by its subclass (`Lookup.findGetter` restricts the receiver
//                 to the lookup class: type `(Sub)int`)
//   special-*     `privateLookupIn(P).findSpecial(P, "m", (), P)`: a handle `(P)int` that does
//                 not select the subclass override
//   static-*      a static getter `(R)int` for toString / hashCode / equals
//   ctor-*        `findConstructor(Q, (Q)V)`: a `(Q)Q` getter
//   wide-*        201 `int` getters (more than 200 slots: `makeToString`
//                 splits the concatenation)
//
// Run: javac -d out L4W43ObjectMethodsJdkRoute.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43ObjectMethodsJdkRoute
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   protected-type: (Sub)int
//   protected-tostring: Sub[count=3]
//   special-type: (P)int
//   special-tostring: P[m=1]
//   special-hashcode: 1
//   static-tostring: R[a=700]
//   static-hashcode: true
//   static-equals: true false
//   ctor-tostring: Q[q=q2]
//   wide-tostring: 1499 1609393848
//   wide-hashcode: true
//
// Before wave 43 every `*-tostring` row failed on CratonVM in
// `MethodHandle.copyWith` (`AbstractMethodError`, inside a
// `RuntimeException` from `makeToString`). The `*-type` rows read how
// CratonVM's `Lookup` types a protected / special handle; the `*-hashcode` /
// `static-equals` rows are the JDK route without concatenation.
import java.io.ByteArrayOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.runtime.ObjectMethods;
import java.util.Arrays;

public class L4W43ObjectMethodsJdkRoute {
    static class Sub extends ByteArrayOutputStream {
        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    static class P {
        int m() {
            return 1;
        }
    }

    static class PSub extends P {
        @Override
        int m() {
            return 2;
        }
    }

    record R(int a) {
        static int sa(R r) {
            return r.a * 100;
        }
    }

    static class Q {
        final int n;

        Q(int n) {
            this.n = n;
        }

        Q(Q other) {
            this.n = other.n + 1;
        }

        @Override
        public String toString() {
            return "q" + n;
        }
    }

    record W(int a) {
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder();
            for (Throwable c = t; c != null; c = c.getCause()) {
                if (sb.length() > 0) {
                    sb.append(" <- ");
                }
                sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
            }
            out = sb.toString();
        }
        System.out.println(name + ": " + out);
    }

    static String simple(MethodType t) {
        return t.toString().replace("L4W43ObjectMethodsJdkRoute$", "");
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandles.Lookup subLookup = Sub.lookup();
        row("protected-type", () -> simple(subLookup.findGetter(ByteArrayOutputStream.class, "count", int.class)
                .type()));
        row("protected-tostring", () -> {
            MethodHandle g = subLookup.findGetter(ByteArrayOutputStream.class, "count", int.class);
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(subLookup, "toString", MethodHandle.class,
                    Sub.class, "count", g);
            Sub s = new Sub();
            s.write(1);
            s.write(2);
            s.write(3);
            return (String) ts.invokeExact(s);
        });
        MethodHandles.Lookup pLookup = MethodHandles.privateLookupIn(P.class, lookup);
        MethodHandle special = pLookup.findSpecial(P.class, "m", MethodType.methodType(int.class), P.class);
        row("special-type", () -> simple(special.type()));
        row("special-tostring", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    P.class, "m", special);
            return (String) ts.invokeExact((P) new PSub());
        });
        row("special-hashcode", () -> {
            MethodHandle hc = (MethodHandle) ObjectMethods.bootstrap(lookup, "hashCode", MethodHandle.class,
                    P.class, "m", special);
            return (int) hc.invokeExact((P) new PSub());
        });
        MethodHandle sa = lookup.findStatic(R.class, "sa", MethodType.methodType(int.class, R.class));
        row("static-tostring", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    R.class, "a", sa);
            return (String) ts.invokeExact(new R(7));
        });
        row("static-hashcode", () -> {
            MethodHandle hc = (MethodHandle) ObjectMethods.bootstrap(lookup, "hashCode", MethodHandle.class,
                    R.class, "a", sa);
            return (int) hc.invokeExact(new R(7)) == Integer.hashCode(700);
        });
        row("static-equals", () -> {
            MethodHandle eq = (MethodHandle) ObjectMethods.bootstrap(lookup, "equals", MethodHandle.class,
                    R.class, "a", sa);
            return (boolean) eq.invokeExact(new R(7), (Object) new R(7)) + " "
                    + (boolean) eq.invokeExact(new R(7), (Object) new R(8));
        });
        row("ctor-tostring", () -> {
            MethodHandle c = lookup.findConstructor(Q.class, MethodType.methodType(void.class, Q.class));
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    Q.class, "q", c);
            return (String) ts.invokeExact(new Q(1));
        });
        MethodHandle wa = lookup.findGetter(W.class, "a", int.class);
        MethodHandle[] wide = new MethodHandle[201];
        Arrays.fill(wide, wa);
        String[] names = new String[201];
        for (int i = 0; i < names.length; i++) {
            names[i] = "a" + i;
        }
        row("wide-tostring", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    W.class, String.join(";", names), wide);
            String s = (String) ts.invokeExact(new W(5));
            return s.length() + " " + s.hashCode();
        });
        row("wide-hashcode", () -> {
            MethodHandle hc = (MethodHandle) ObjectMethods.bootstrap(lookup, "hashCode", MethodHandle.class,
                    W.class, String.join(";", names), wide);
            int expect = 0;
            for (int i = 0; i < 201; i++) {
                expect = expect * 31 + Integer.hashCode(5);
            }
            return (int) hc.invokeExact(new W(5)) == expect;
        });
    }
}
