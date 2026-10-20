// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4 (bug hunt): an adapted
// `MethodHandles.identity` converts its argument to its own type. CratonVM's
// `MH_KIND_IDENTITY` arm returned the argument as it came, so an
// `explicitCastArguments` copy neither narrowed nor unboxed nor cast:
// `ConstantBootstraps.explicitCast(lookup, "c", byte.class, 300)` (whose JDK
// body is `explicitCastArguments(identity(dstType), (Object)dstType)`)
// answered `300` where HotSpot answers `44`, and `explicitCast(String, 3)`
// answered `3` where HotSpot throws.
//
// Positive control: row `condy-byte` (the base printed `300`).
//
// Run: javac -d out L4W48IdentityConversions.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48IdentityConversions
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   condy-byte: 44
//   condy-string: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   condy-char: A
//   condy-boolean: false
//   condy-object: 3
//   explicit-byte: 44
//   explicit-short-from-long: -1
//   explicit-null-int: 0
//   astype-long: 5
//   astype-cce: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   astype-npe: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
//   plain: x
import java.lang.invoke.ConstantBootstraps;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W48IdentityConversions {
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

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        row("condy-byte", () -> ConstantBootstraps.explicitCast(l, "c", byte.class, 300));
        row("condy-string", () -> ConstantBootstraps.explicitCast(l, "c", String.class, 3));
        row("condy-char", () -> ConstantBootstraps.explicitCast(l, "c", char.class, 65));
        row("condy-boolean", () -> ConstantBootstraps.explicitCast(l, "c", boolean.class, 2));
        row("condy-object", () -> ConstantBootstraps.explicitCast(l, "c", Object.class, 3));
        row("explicit-byte", () -> {
            MethodHandle mh = MethodHandles.explicitCastArguments(MethodHandles.identity(byte.class),
                MethodType.methodType(byte.class, Object.class));
            return (byte) mh.invokeExact((Object) 300);
        });
        row("explicit-short-from-long", () -> {
            MethodHandle mh = MethodHandles.explicitCastArguments(MethodHandles.identity(short.class),
                MethodType.methodType(short.class, long.class));
            return (short) mh.invokeExact(0xFFFFFFFFL);
        });
        row("explicit-null-int", () -> {
            MethodHandle mh = MethodHandles.explicitCastArguments(MethodHandles.identity(int.class),
                MethodType.methodType(int.class, Object.class));
            return (int) mh.invokeExact((Object) null);
        });
        row("astype-long", () -> {
            MethodHandle mh = MethodHandles.identity(long.class).asType(MethodType.methodType(long.class, int.class));
            return (long) mh.invokeExact(5);
        });
        row("astype-cce", () -> {
            MethodHandle mh = MethodHandles.identity(int.class).asType(MethodType.methodType(int.class, Object.class));
            return (int) mh.invokeExact((Object) "s");
        });
        row("astype-npe", () -> {
            MethodHandle mh = MethodHandles.identity(int.class).asType(MethodType.methodType(int.class, Object.class));
            return (int) mh.invokeExact((Object) null);
        });
        row("plain", () -> (String) MethodHandles.identity(String.class).invokeExact("x"));
    }
}
