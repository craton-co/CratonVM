// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L4 (proposal
// `i48-L4-proposal-retire-the-reflective-lambdametafactory-bridge`):
// `LambdaMetafactory.metafactory` / `altMetafactory` called DIRECTLY (as
// log4j's `ServiceLoaderUtil` and Spring's `LambdaSafe` do), not through
// `invokedynamic`. CratonVM registers a reflective `Bridge` for both that
// builds a proxy without the JDK's checks. With
// `CRATONVM_LMF_REFLECTIVE_TO_JDK` on, the call runs the JDK's own factory.
// Each linking row runs twice (a first call on a cold VM, then again).
//
// Run: javac -d out L4W49ReflectiveMetafactory.java
//      CRATONVM_LMF_REFLECTIVE_TO_JDK=1 cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W49ReflectiveMetafactory
// The switch is OFF by default (wave 49: see the proposal's "Blocker"); with
// it off this probe differed until wave 50 (`bad-return` panicked the VM;
// `not-interface` and `arity` linked): since wave 50 (lane L4) the bridge
// runs the JDK's checks and unboxes a non-wrapper return through `Number`,
// and the probe matches with the switch off too.
//
// Positive control: `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints one
// `[lmf-reflective] ... -> JDK factory` line per reflective call with the
// switch on, none with `CRATONVM_LMF_REFLECTIVE_TO_JDK=0`.
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   supplier: hi true
//   supplier-again: hi true
//   function: 6
//   not-interface: java.lang.invoke.LambdaConversionException: java.lang.String is not an interface
//   arity: java.lang.invoke.LambdaConversionException: Incorrect number of parameters for static method invokeStatic L4W49ReflectiveMetafactory.hello:()String; 1 captured parameters, 0 functional interface method parameters, 0 implementation parameters
//   alt-serializable: hi true true
//   alt-markers: hi true
//   bad-return: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   supplier-last: hi true
import java.io.Serializable;
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Function;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class L4W49ReflectiveMetafactory {
    interface Marker {
    }

    static String hello() {
        return "hi";
    }

    static Integer twice(Integer x) {
        return x * 2;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    static MethodHandle helloHandle() throws Exception {
        return L.findStatic(L4W49ReflectiveMetafactory.class, "hello", MethodType.methodType(String.class));
    }

    static Object supplier() throws Throwable {
        CallSite cs = LambdaMetafactory.metafactory(L, "get", MethodType.methodType(Supplier.class),
            MethodType.methodType(Object.class), helloHandle(), MethodType.methodType(String.class));
        Supplier<?> s = (Supplier<?>) cs.getTarget().invoke();
        return s.get() + " " + s.getClass().isSynthetic();
    }

    public static void main(String[] args) throws Throwable {
        row("supplier", L4W49ReflectiveMetafactory::supplier);
        row("supplier-again", L4W49ReflectiveMetafactory::supplier);
        row("function", () -> {
            MethodHandle impl = L.findStatic(L4W49ReflectiveMetafactory.class, "twice",
                MethodType.methodType(Integer.class, Integer.class));
            CallSite cs = LambdaMetafactory.metafactory(L, "apply", MethodType.methodType(Function.class),
                MethodType.methodType(Object.class, Object.class), impl,
                MethodType.methodType(Integer.class, Integer.class));
            @SuppressWarnings("unchecked")
            Function<Integer, Integer> f = (Function<Integer, Integer>) cs.getTarget().invoke();
            return f.apply(3);
        });
        row("not-interface", () -> LambdaMetafactory.metafactory(L, "get", MethodType.methodType(String.class),
            MethodType.methodType(Object.class), helloHandle(), MethodType.methodType(String.class)));
        row("arity", () -> LambdaMetafactory.metafactory(L, "get",
            MethodType.methodType(Supplier.class, String.class),
            MethodType.methodType(Object.class), helloHandle(), MethodType.methodType(String.class)));
        row("alt-serializable", () -> {
            CallSite cs = LambdaMetafactory.altMetafactory(L, "get", MethodType.methodType(Supplier.class),
                MethodType.methodType(Object.class), helloHandle(), MethodType.methodType(String.class),
                LambdaMetafactory.FLAG_SERIALIZABLE);
            Supplier<?> s = (Supplier<?>) cs.getTarget().invoke();
            return s.get() + " " + (s instanceof Serializable) + " " + s.getClass().isSynthetic();
        });
        row("alt-markers", () -> {
            CallSite cs = LambdaMetafactory.altMetafactory(L, "get", MethodType.methodType(Supplier.class),
                MethodType.methodType(Object.class), helloHandle(), MethodType.methodType(String.class),
                LambdaMetafactory.FLAG_MARKERS, 1, Marker.class);
            Supplier<?> s = (Supplier<?>) cs.getTarget().invoke();
            return s.get() + " " + (s instanceof Marker);
        });
        row("bad-return", () -> {
            CallSite cs = LambdaMetafactory.metafactory(L, "getAsInt",
                MethodType.methodType(IntSupplier.class), MethodType.methodType(int.class), helloHandle(),
                MethodType.methodType(int.class));
            IntSupplier s = (IntSupplier) cs.getTarget().invoke();
            return s.getAsInt();
        });
        row("supplier-last", L4W49ReflectiveMetafactory::supplier);
    }
}
