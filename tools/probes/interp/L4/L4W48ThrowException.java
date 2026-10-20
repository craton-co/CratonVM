// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4: `MethodHandles.throwException`
// (`docs/internal/fixed-bugs/interpreter-L4-methodhandles-throwexception-fails-on-the-methodtype-form-FIXED-20261012.md`). The
// JDK body builds a `LambdaForm` over the `MethodType`'s `form`, which
// CratonVM's synthetic `MethodType` does not carry: every row failed with
// `NullPointerException: Cannot invoke "java.lang.invoke.MethodTypeForm.basicType()"
// because "this.form" is null` before wave 48.
//
// Positive control: row `void` (the base printed the `this.form` NPE).
//
// Run: javac -d out L4W48ThrowException.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48ThrowException
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   void: java.lang.IllegalStateException: boom
//   int-exact: java.lang.RuntimeException: x
//   type: (Exception)String
//   string: MethodHandle(Exception)String
//   bound: java.lang.Exception: b
//   bound-type: ()String
//   null-arg: java.lang.NullPointerException: Cannot throw exception because "t" is null
//   not-throwable: java.lang.ClassCastException: java.lang.String
//   null-rtype: java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "this.rtype" is null
//   null-extype: java.lang.NullPointerException: null
//   rethrow: java.lang.IllegalStateException: inner
//   subclass: java.lang.IllegalArgumentException: sub
//   wrong-class: java.lang.ClassCastException: Cannot cast java.lang.RuntimeException to java.lang.IllegalStateException
//   reveal: java.lang.IllegalArgumentException: not a direct method handle
//   guard: java.lang.UnsupportedOperationException: no
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W48ThrowException {
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

    static void inner() {
        throw new IllegalStateException("inner");
    }

    static String ok(int x) {
        return "ok" + x;
    }

    static boolean isPositive(int x) {
        return x > 0;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        row("void", () -> {
            MethodHandle mh = MethodHandles.throwException(void.class, IllegalStateException.class);
            mh.invoke(new IllegalStateException("boom"));
            return "returned";
        });
        row("int-exact", () -> {
            MethodHandle mh = MethodHandles.throwException(int.class, RuntimeException.class);
            int r = (int) mh.invokeExact(new RuntimeException("x"));
            return r;
        });
        row("type", () -> MethodHandles.throwException(String.class, Exception.class).type());
        row("string", () -> MethodHandles.throwException(String.class, Exception.class).toString());
        row("bound", () -> {
            MethodHandle mh = MethodHandles.throwException(String.class, Exception.class)
                .bindTo(new Exception("b"));
            return (String) mh.invokeExact();
        });
        row("bound-type", () -> MethodHandles.throwException(String.class, Exception.class)
            .bindTo(new Exception("b")).type());
        row("null-arg", () -> {
            MethodHandle mh = MethodHandles.throwException(void.class, RuntimeException.class);
            mh.invoke((RuntimeException) null);
            return "returned";
        });
        row("not-throwable", () -> MethodHandles.throwException(void.class, (Class) String.class));
        row("null-rtype", () -> MethodHandles.throwException(null, RuntimeException.class));
        row("null-extype", () -> MethodHandles.throwException(void.class, null));
        row("rethrow", () -> {
            MethodHandle target = l.findStatic(L4W48ThrowException.class, "inner", MethodType.methodType(void.class));
            MethodHandle handler = MethodHandles.throwException(void.class, IllegalStateException.class);
            MethodHandle mh = MethodHandles.catchException(target, IllegalStateException.class, handler);
            mh.invokeExact();
            return "returned";
        });
        row("subclass", () -> {
            MethodHandle mh = MethodHandles.throwException(void.class, RuntimeException.class);
            mh.invoke(new IllegalArgumentException("sub"));
            return "returned";
        });
        row("wrong-class", () -> {
            MethodHandle mh = MethodHandles.throwException(void.class, IllegalStateException.class);
            mh.invoke((Object) new RuntimeException("w"));
            return "returned";
        });
        row("reveal", () -> l.revealDirect(MethodHandles.throwException(void.class, Exception.class)));
        row("guard", () -> {
            MethodHandle test = l.findStatic(L4W48ThrowException.class, "isPositive",
                MethodType.methodType(boolean.class, int.class));
            MethodHandle target = l.findStatic(L4W48ThrowException.class, "ok",
                MethodType.methodType(String.class, int.class));
            MethodHandle fail = MethodHandles.dropArguments(
                MethodHandles.throwException(String.class, UnsupportedOperationException.class)
                    .bindTo(new UnsupportedOperationException("no")),
                0, int.class);
            MethodHandle mh = MethodHandles.guardWithTest(test, target, fail);
            return (String) mh.invokeExact(1) + " " + (String) mh.invokeExact(-1);
        });
    }
}
