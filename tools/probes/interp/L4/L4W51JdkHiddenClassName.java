// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L4: a hidden class the JDK's own
// `Lookup.ClassDefiner` defines (through `JavaLangAccess.defineClass`) is
// named `<this_class>/0x<id>`, as HotSpot names it. The reflective
// `LambdaMetafactory` route runs the JDK's factory when
// `CRATONVM_LMF_REFLECTIVE_TO_JDK` is on (the default since wave 51), and
// before wave 51 its proxy was `Host$$Lambda` with no tail (page
// `docs/internal/fixed-bugs/interpreter-i48-L4-proposal-retire-the-reflective-lambdametafactory-bridge-DONE-20261014.md`,
// "Blocker"). `/0x<id>` masks the address; `tail-lengths` prints its
// digit count (HotSpot prints the address as 16 hex digits; CratonVM's
// counter tails had one or two before wave 51). The first row runs on a cold
// VM; `metafactory-again` repeats it last.
//
// Run: javac -d out L4W51JdkHiddenClassName.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W51JdkHiddenClassName
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   metafactory: L4W51JdkHiddenClassName$$Lambda/0x<id> hidden=true nest=L4W51JdkHiddenClassName simple=L4W51JdkHiddenClassName$$Lambda/0x<id> value=5
//   alt-metafactory: L4W51JdkHiddenClassName$$Lambda/0x<id> hidden=true nest=L4W51JdkHiddenClassName simple=L4W51JdkHiddenClassName$$Lambda/0x<id> value=3
//   two-classes: true
//   define-hidden: L4W51JdkHiddenClassName$Shape/0x<id> hidden=true nest=L4W51JdkHiddenClassName
//   tail-lengths: 16 16
//   to-string: L4W51JdkHiddenClassName$$Lambda/0x<id>@<hash>
//   metafactory-again: L4W51JdkHiddenClassName$$Lambda/0x<id> hidden=true nest=L4W51JdkHiddenClassName simple=L4W51JdkHiddenClassName$$Lambda/0x<id> value=5
import java.io.InputStream;
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.ToIntFunction;

public class L4W51JdkHiddenClassName {
    static final MethodType FACTORY = MethodType.methodType(ToIntFunction.class);
    static final MethodType SAM = MethodType.methodType(int.class, Object.class);
    static final MethodType INSTANTIATED = MethodType.methodType(int.class, String.class);

    static class Shape {
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static String mask(String s) {
        return s.replaceAll("/0x[0-9a-f]+", "/0x<id>");
    }

    static int tail(Class<?> c) {
        String n = c.getName();
        int at = n.lastIndexOf("/0x");
        return at < 0 ? -1 : n.length() - at - 3;
    }

    static Class<?> defineHidden() throws Throwable {
        byte[] bytes;
        try (InputStream in = L4W51JdkHiddenClassName.class
                .getResourceAsStream("L4W51JdkHiddenClassName$Shape.class")) {
            bytes = in.readAllBytes();
        }
        return MethodHandles.lookup()
            .defineHiddenClass(bytes, false, MethodHandles.Lookup.ClassOption.NESTMATE)
            .lookupClass();
    }

    static String describe(Class<?> c) {
        return mask(c.getName()) + " hidden=" + c.isHidden() + " nest=" + c.getNestHost().getName();
    }

    @SuppressWarnings("unchecked")
    static ToIntFunction<String> metafactory() throws Throwable {
        MethodHandle length = MethodHandles.lookup().findVirtual(String.class, "length",
            MethodType.methodType(int.class));
        CallSite cs = LambdaMetafactory.metafactory(MethodHandles.lookup(), "applyAsInt",
            FACTORY, SAM, length, INSTANTIATED);
        return (ToIntFunction<String>) cs.getTarget().invoke();
    }

    @SuppressWarnings("unchecked")
    static ToIntFunction<String> altMetafactory() throws Throwable {
        MethodHandle length = MethodHandles.lookup().findVirtual(String.class, "length",
            MethodType.methodType(int.class));
        CallSite cs = LambdaMetafactory.altMetafactory(MethodHandles.lookup(), "applyAsInt",
            FACTORY, SAM, length, INSTANTIATED, LambdaMetafactory.FLAG_SERIALIZABLE);
        return (ToIntFunction<String>) cs.getTarget().invoke();
    }

    static String lambdaRow(ToIntFunction<String> f, String arg) {
        Class<?> c = f.getClass();
        return describe(c) + " simple=" + mask(c.getSimpleName()) + " value=" + f.applyAsInt(arg);
    }

    public static void main(String[] args) {
        row("metafactory", () -> lambdaRow(metafactory(), "hello"));
        row("alt-metafactory", () -> lambdaRow(altMetafactory(), "abc"));
        row("two-classes", () -> metafactory().getClass() != altMetafactory().getClass());
        row("define-hidden", () -> describe(defineHidden()));
        row("tail-lengths", () -> tail(metafactory().getClass()) + " " + tail(defineHidden()));
        row("to-string", () -> mask(metafactory().toString()).replaceAll("@[0-9a-f]+$", "@<hash>"));
        row("metafactory-again", () -> lambdaRow(metafactory(), "hello"));
    }
}
