// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L3: which classes an agent may
// retransform (`Instrumentation.isModifiableClass`, and what
// `retransformClasses` / `redefineClasses` say about the rest). One row per
// class: `modifiable=<isModifiableClass>` and `retransform=<ok | the
// exception's simple class name>`.
//   lambda  -- the class of a lambda object (HotSpot: a hidden class);
//   mref    -- the class of a method-reference object;
//   hidden  -- a class `Lookup.defineHiddenClass` defined;
//   array   -- `int[].class`;
//   prim    -- `int.class`;
//   plain   -- this probe's nested class `Plain`.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     lambda modifiable=false retransform=UnmodifiableClassException
//     mref modifiable=false retransform=UnmodifiableClassException
//     hidden modifiable=false retransform=UnmodifiableClassException
//     array modifiable=false retransform=UnmodifiableClassException
//     prim modifiable=false retransform=UnmodifiableClassException
//     plain modifiable=true retransform=ok
// CratonVM before wave 40, read from the code (not run): `lambda` and `mref`
// printed `modifiable=true` and did not throw UnmodifiableClassException when
// the proxy was one the VM minted for its native lambda dispatch (a class id
// at or above `LAMBDA_PROXY_ID_BASE`, not in the class store):
// `instrument::mirror_classification` asked only the class store's hidden
// flag, although `Class.isHidden` answers `true` for such a proxy. Since wave
// 40 it asks what `Class.isHidden` asks. A proxy the JDK spun itself
// (`InnerClassLambdaMetafactory`, a real hidden class) was right already.
// --compatible prints the same lines.
// Positive control: the `lambda` row itself; `Class.isHidden()` of the
// lambda's class prints `true` on both VMs.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W40ModifiableLambdaClass$Agent
//     Can-Retransform-Classes: true
// containing L3W40ModifiableLambdaClass*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W40ModifiableLambdaClass
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class L3W40ModifiableLambdaClass {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Plain {
        public static int value() {
            return 1;
        }
    }

    /** Defined again as a hidden class. */
    public static class Hid {
        public static int value() {
            return 2;
        }
    }

    static String row(Instrumentation i, String name, Class<?> c) {
        String retransform;
        try {
            i.retransformClasses(c);
            retransform = "ok";
        } catch (Throwable t) {
            retransform = t.getClass().getSimpleName();
        }
        return name + " modifiable=" + i.isModifiableClass(c) + " retransform=" + retransform;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        IntSupplier lambda = () -> 3;
        Supplier<String> mref = String::new;
        byte[] hidBytes;
        try (InputStream in = L3W40ModifiableLambdaClass.class
                .getResourceAsStream("L3W40ModifiableLambdaClass$Hid.class")) {
            hidBytes = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(hidBytes, true).lookupClass();
        System.out.println(row(i, "lambda", lambda.getClass()) + (lambda.getAsInt() == 3 ? "" : " wrong"));
        System.out.println(row(i, "mref", mref.getClass()) + (mref.get().isEmpty() ? "" : " wrong"));
        System.out.println(row(i, "hidden", hidden));
        System.out.println(row(i, "array", int[].class));
        System.out.println(row(i, "prim", int.class));
        System.out.println(row(i, "plain", Plain.class) + (Plain.value() == 1 ? "" : " wrong"));
    }
}
