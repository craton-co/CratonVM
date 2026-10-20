// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L4 -- defect
// `i60-L3-the-jdk-accessor-cannot-call-a-serializable-lambdas-writereplace`:
// a method handle on an INSTANCE member of a lambda proxy class (the
// serializable lambda's private `writeReplace`, its public functional method)
// dropped the receiver parameter -- `unreflect` / `findVirtual` answered
// `()Object` where HotSpot answers `(P$$Lambda)Object` -- so the JDK's
// reflective accessor (`CRATONVM_JDK_REFLECTIVE_INVOKE=1`) failed every
// serializable lambda's serialization with `WrongMethodTypeException`.
// Every row prints the handle's arity, whether parameter 0 is the proxy
// class, and the answer of a call; the refusal rows print HotSpot's texts
// (the proxy's hidden-class name is masked as `P`: its suffix is an address).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// unreflect-writeReplace: 1 true SerializedLambda true run 0
// findVirtual-writeReplace: 1 true SerializedLambda true run 0
// findSpecial-writeReplace: 1 true SerializedLambda true run 0
// bind-writeReplace: 0 false SerializedLambda true run 0
// unreflect-run: 1 true ran 1
// findVirtual-run: 1 true ran 2
// bind-run: 0 false ran 3
// findVirtual-toString: 1 true true
// host-lookup-writeReplace: 1 true SerializedLambda true run 0
// findVirtual-absent: java.lang.NoSuchMethodException: no such method: P.nope()void/invokeVirtual
// findStatic-instance: java.lang.IllegalAccessException: no such method: P.run()void/invokeStatic
// findVirtual-wrong-type: java.lang.NoSuchMethodException: no such method: P.run()int/invokeVirtual
// reflect-writeReplace: SerializedLambda true run 0
// serialized-lambda: deserialized ran 4
// unreflect-writeReplace-again: 1 true SerializedLambda true run 0
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W61LambdaProxyMemberHandles`,
// and again with `CRATONVM_JDK_REFLECTIVE_INVOKE=1` (same expected output).
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.SerializedLambda;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W61LambdaProxyMemberHandles {
    interface Row {
        Object run() throws Throwable;
    }

    /** The proxy's `getName()`, masked as `P` (its suffix is an address). */
    static String proxyName = "\0";

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException && t.getCause() != null ? t.getCause() : t;
            out = String.valueOf(c);
        }
        System.out.println(name + ": " + out.replace(proxyName, "P"));
    }

    static long runs;

    static final class Holder {
        static Runnable make() {
            return (Runnable & Serializable) () -> runs++;
        }
    }

    static String describe(MethodHandle h, Class<?> proxy, Object answer) {
        MethodType t = h.type();
        boolean receiver = t.parameterCount() > 0 && t.parameterType(0) == proxy;
        return t.parameterCount() + " " + receiver + " " + answer(answer);
    }

    static String answer(Object answer) {
        if (answer instanceof SerializedLambda sl) {
            // javac names a serializable lambda `lambda$make$<hash>$<n>`.
            return "SerializedLambda " + sl.getImplMethodName().startsWith("lambda$make$") + " "
                    + sl.getFunctionalInterfaceMethodName() + " " + sl.getCapturedArgCount();
        }
        return answer == null ? "ran " + runs : String.valueOf(answer);
    }

    static Object call(MethodHandle h, Runnable r) throws Throwable {
        Object out;
        if (h.type().parameterCount() == 0) {
            out = h.type().returnType() == void.class ? null : h.invoke();
            if (h.type().returnType() == void.class) {
                h.invoke();
            }
        } else {
            out = h.type().returnType() == void.class ? null : h.invoke(r);
            if (h.type().returnType() == void.class) {
                h.invoke(r);
            }
        }
        return out;
    }

    public static void main(String[] args) throws Throwable {
        Runnable r = Holder.make();
        Class<?> proxy = r.getClass();
        proxyName = proxy.getName();
        MethodHandles.Lookup l = MethodHandles.privateLookupIn(proxy, MethodHandles.lookup());
        MethodType wr = MethodType.methodType(Object.class);
        row("unreflect-writeReplace", () -> {
            Method m = proxy.getDeclaredMethod("writeReplace");
            MethodHandle h = l.unreflect(m);
            return describe(h, proxy, call(h, r));
        });
        row("findVirtual-writeReplace", () -> {
            MethodHandle h = l.findVirtual(proxy, "writeReplace", wr);
            return describe(h, proxy, call(h, r));
        });
        row("findSpecial-writeReplace", () -> {
            MethodHandle h = l.findSpecial(proxy, "writeReplace", wr, proxy);
            return describe(h, proxy, call(h, r));
        });
        row("bind-writeReplace", () -> {
            MethodHandle h = l.bind(r, "writeReplace", wr);
            return describe(h, proxy, call(h, r));
        });
        row("unreflect-run", () -> {
            MethodHandle h = l.unreflect(proxy.getDeclaredMethod("run"));
            return describe(h, proxy, call(h, r));
        });
        row("findVirtual-run", () -> {
            MethodHandle h = l.findVirtual(proxy, "run", MethodType.methodType(void.class));
            return describe(h, proxy, call(h, r));
        });
        row("bind-run", () -> {
            MethodHandle h = l.bind(r, "run", MethodType.methodType(void.class));
            return describe(h, proxy, call(h, r));
        });
        row("findVirtual-toString", () -> {
            MethodHandle h = l.findVirtual(proxy, "toString", MethodType.methodType(String.class));
            String s = (String) h.invoke(r);
            return describe(h, proxy, s.startsWith(proxy.getName() + "@"));
        });
        row("host-lookup-writeReplace", () -> {
            MethodHandle h = MethodHandles.lookup().findVirtual(proxy, "writeReplace", wr);
            return describe(h, proxy, call(h, r));
        });
        row("findVirtual-absent", () -> l.findVirtual(proxy, "nope", MethodType.methodType(void.class)));
        row("findStatic-instance", () -> l.findStatic(proxy, "run", MethodType.methodType(void.class)));
        row("findVirtual-wrong-type", () -> l.findVirtual(proxy, "run", MethodType.methodType(int.class)));
        row("reflect-writeReplace", () -> {
            Method m = proxy.getDeclaredMethod("writeReplace");
            m.setAccessible(true);
            return answer(m.invoke(r));
        });
        row("serialized-lambda", () -> {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
                out.writeObject(r);
            }
            try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
                Runnable back = (Runnable) in.readObject();
                back.run();
            }
            return "deserialized ran " + runs;
        });
        row("unreflect-writeReplace-again", () -> {
            MethodHandle h = l.unreflect(proxy.getDeclaredMethod("writeReplace"));
            return describe(h, proxy, call(h, r));
        });
    }
}
