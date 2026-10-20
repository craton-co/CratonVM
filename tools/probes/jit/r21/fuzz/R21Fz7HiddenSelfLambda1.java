// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 21, lane review-w21: a hidden class's references to its
// own class-file name from a LAMBDA body and a METHOD REFERENCE, hot enough to
// compile, before and after the ordinary class of the same bytes is loaded.
//
// Risk targeted (docs/known-issues/jit/r21-review-w21-findings-20261019.md,
// items 1-3): wave 20 (`CRATONVM_HIDDEN_SELF_CP_BY_ID`) binds a hidden class's
// own-name `Fieldref` / `Methodref` owners to the hidden class itself in the
// interpreter and the JIT site doors. The lambda's implementation method
// (`lambda$run$0`, private static in the hidden class) is reached through a
// `MethodHandle` constant whose owner is the class's own name, and its body
// does `new <ITSELF>`, `invokespecial <ITSELF>.<init>`, `invokevirtual
// <ITSELF>.priv` and `invokestatic <ITSELF>.step` -- the R21Fz5HiddenOsr1
// shapes, one indirection further away. `selfIs` is `ldc <ITSELF>`.
//
// Run:
//   javac -d out R21Fz7HiddenSelfLambda1.java
//   java -cp out R21Fz7HiddenSelfLambda1                     # HotSpot 25
//   cratonvm -cp out R21Fz7HiddenSelfLambda1                 # --jdk-only
//   cratonvm --compatible -cp out R21Fz7HiddenSelfLambda1
//   cratonvm --nojit -cp out R21Fz7HiddenSelfLambda1
//   CRATONVM_HIDDEN_SELF_CP_BY_ID=0 cratonvm ...             # control arm
// HotSpot expectation (by reading JVMS 5.4.3.1 and Lookup::defineHiddenClass,
// not yet run):
//   h1 run=40000 count=20000 selfIs(h1)=true selfIs(plain)=false
//   plain run=10 count=5 selfIs(h1)=false selfIs(plain)=true
//   h1 again run=40000 count=40000
//   h2 run=6 count=3 selfIs(h2)=true selfIs(h1)=false
//   final h1=40000 h2=3 plain=5

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

public class R21Fz7HiddenSelfLambda1 {
    static final class Hidden {
        final MethodHandle run;
        final MethodHandle count;
        final MethodHandle selfIs;
        final Class<?> c;

        Hidden(MethodHandles.Lookup l) throws ReflectiveOperationException {
            c = l.lookupClass();
            run = l.findStatic(c, "run", MethodType.methodType(int.class, int.class));
            count = l.findStatic(c, "count", MethodType.methodType(int.class));
            selfIs = l.findStatic(c, "selfIs", MethodType.methodType(boolean.class, Class.class));
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in =
                R21Fz7HiddenSelfLambda1.class.getResourceAsStream("R21Fz7HsPayload.class")) {
            bytes = in.readAllBytes();
        }
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Hidden h1 = new Hidden(lk.defineHiddenClass(bytes, true));
        int r = (int) h1.run.invokeExact(20000);
        System.out.println("h1 run=" + r + " count=" + (int) h1.count.invokeExact()
                + " selfIs(h1)=" + (boolean) h1.selfIs.invokeExact((Class<?>) h1.c)
                + " selfIs(plain)=" + (boolean) h1.selfIs.invokeExact((Class<?>) R21Fz7HsPayload.class));

        // The ordinary class of the same bytes, from the class path.
        r = R21Fz7HsPayload.run(5);
        System.out.println("plain run=" + r + " count=" + R21Fz7HsPayload.count()
                + " selfIs(h1)=" + R21Fz7HsPayload.selfIs(h1.c)
                + " selfIs(plain)=" + R21Fz7HsPayload.selfIs(R21Fz7HsPayload.class));

        r = (int) h1.run.invokeExact(20000);
        System.out.println("h1 again run=" + r + " count=" + (int) h1.count.invokeExact());

        Hidden h2 = new Hidden(lk.defineHiddenClass(bytes, true));
        r = (int) h2.run.invokeExact(3);
        System.out.println("h2 run=" + r + " count=" + (int) h2.count.invokeExact()
                + " selfIs(h2)=" + (boolean) h2.selfIs.invokeExact((Class<?>) h2.c)
                + " selfIs(h1)=" + (boolean) h2.selfIs.invokeExact((Class<?>) h1.c));

        System.out.println("final h1=" + (int) h1.count.invokeExact()
                + " h2=" + (int) h2.count.invokeExact()
                + " plain=" + R21Fz7HsPayload.count());
    }
}

// The bytes the hidden classes are defined from, and the ordinary class of the
// same name. No method or field descriptor names the class itself, so only
// CONSTANT_Class entries (which JVMS resolves to the hidden class) refer to it.
class R21Fz7HsPayload {
    static int sCount;

    private int priv() {
        return 1;
    }

    private static int step(int x) {
        return x + 1;
    }

    static boolean selfIs(Class<?> c) {
        return R21Fz7HsPayload.class == c;
    }

    static int count() {
        return sCount;
    }

    static int run(int n) {
        // A lambda whose body names the class itself, and a method reference
        // to its own private static method.
        IntUnaryOperator f = x -> step(x) + new R21Fz7HsPayload().priv();
        IntUnaryOperator g = R21Fz7HsPayload::step;
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc = f.applyAsInt(acc) - g.applyAsInt(0) + 1;
            sCount++;
        }
        return acc;
    }
}
