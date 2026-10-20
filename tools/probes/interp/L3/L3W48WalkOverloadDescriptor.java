// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L3 -- review: a `StackWalker` frame of an
// OVERLOADED method reports that overload's own descriptor and method type.
// Every accessor of this VM's two frame carriers (`StackFrameInfo`'s
// `getMethodType` / `getDescriptor` / `expandStackFrameInfo`, the native
// walk's `StackWalker$StackFrame`) resolved the descriptor from the frame's
// class and method NAME and took the first declared overload, so a frame of
// `f(String)` answered `(I)V` when `f(int)` is declared first. HotSpot
// reports the frame's method.
//
// Each row walks with RETAIN_CLASS_REFERENCE from inside one overload (the
// first, the second, a static one sharing its name with an instance one, a
// constructor of a class with two) and prints the innermost frame of the
// probe's class: its method name, `getDescriptor()` and
// `getMethodType().toMethodDescriptorString()`. `forEach` rows use the
// `StackWalker.forEach` path, `walk` rows `StackWalker.walk`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// walk f(int): f (I)V (I)V
// walk f(String): f (Ljava/lang/String;)V (Ljava/lang/String;)V
// walk f(long,Object): f (JLjava/lang/Object;)Ljava/lang/String; (JLjava/lang/Object;)Ljava/lang/String;
// walk g(): g ()V ()V
// walk g(int) static: g (I)V (I)V
// walk <init>(String): <init> (Ljava/lang/String;)V (Ljava/lang/String;)V
// forEach f(int): f (I)V (I)V
// forEach f(String): f (Ljava/lang/String;)V (Ljava/lang/String;)V
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 48 (read from the code, then run locally): every
// second overload printed the first's descriptor, e.g.
// `walk f(String): f (I)V (I)V`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W48WalkOverloadDescriptor`.
import java.util.EnumSet;

public class L3W48WalkOverloadDescriptor {
    static final StackWalker W = StackWalker.getInstance(
            EnumSet.of(StackWalker.Option.RETAIN_CLASS_REFERENCE));
    static boolean useForEach;
    static String result;

    static void walk() {
        if (useForEach) {
            StringBuilder sb = new StringBuilder();
            W.forEach(f -> {
                if (sb.length() == 0 && f.getDeclaringClass() == L3W48WalkOverloadDescriptor.class
                        && !f.getMethodName().equals("walk")) {
                    sb.append(f.getMethodName()).append(' ').append(f.getDescriptor()).append(' ')
                            .append(f.getMethodType().toMethodDescriptorString());
                }
            });
            result = sb.toString();
            return;
        }
        result = W.walk(s -> s
                .filter(f -> f.getDeclaringClass() == L3W48WalkOverloadDescriptor.class
                        && !f.getMethodName().equals("walk"))
                .findFirst()
                .map(f -> f.getMethodName() + " " + f.getDescriptor() + " "
                        + f.getMethodType().toMethodDescriptorString())
                .orElse("none"));
    }

    L3W48WalkOverloadDescriptor() {
        walk();
    }

    L3W48WalkOverloadDescriptor(String s) {
        walk();
    }

    void f(int x) {
        walk();
    }

    void f(String s) {
        walk();
    }

    String f(long a, Object b) {
        walk();
        return "";
    }

    void g() {
        walk();
    }

    static void g(int x) {
        walk();
    }

    public static void main(String[] args) {
        L3W48WalkOverloadDescriptor p = new L3W48WalkOverloadDescriptor();
        for (int mode = 0; mode < 2; mode++) {
            useForEach = mode == 1;
            String tag = useForEach ? "forEach " : "walk ";
            p.f(1);
            System.out.println(tag + "f(int): " + result);
            p.f("s");
            System.out.println(tag + "f(String): " + result);
            if (useForEach) {
                continue;
            }
            p.f(1L, null);
            System.out.println(tag + "f(long,Object): " + result);
            p.g();
            System.out.println(tag + "g(): " + result);
            g(1);
            System.out.println(tag + "g(int) static: " + result);
            new L3W48WalkOverloadDescriptor("s");
            System.out.println(tag + "<init>(String): " + result);
        }
    }
}
