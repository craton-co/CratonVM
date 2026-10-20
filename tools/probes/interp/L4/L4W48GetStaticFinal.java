// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4 (bug hunt; page
// `docs/internal/fixed-bugs/interpreter-L4-constantbootstraps-getstaticfinal-reads-no-membername-FIXED-20261013.md`):
// `ConstantBootstraps.getStaticFinal`, called directly. Its JDK body is
// `lookup.findStaticGetter(..)` then `mh.internalMemberName().isFinal()`;
// CratonVM's handles answered `internalMemberName()` with `null`, so every row
// was `NullPointerException: Cannot invoke "java.lang.invoke.MemberName.isFinal()"
// because "member" is null`. (A `CONSTANT_Dynamic` naming it is answered by
// `condy_fast_answer` without the JDK body.) Fixed in wave 49 (lane L4).
//
// Run: javac -d out L4W48GetStaticFinal.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48GetStaticFinal
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   string: sf
//   int: 42
//   boxed: 7
//   not-final: java.lang.IncompatibleClassChangeError: not a final field: notFinal
//   missing: java.lang.NoSuchFieldError: Class L4W48GetStaticFinal does not have member field 'java.lang.String nope'
//   three-arg: true
//   jdk: 2147483647
import java.lang.invoke.ConstantBootstraps;
import java.lang.invoke.MethodHandles;

public class L4W48GetStaticFinal {
    static final String SF = "sf";
    static final int SI = 42;
    static String notFinal = "nf";
    static final Integer BOXED = 7;

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
        Class<?> self = L4W48GetStaticFinal.class;
        row("string", () -> ConstantBootstraps.getStaticFinal(l, "SF", String.class, self));
        row("int", () -> ConstantBootstraps.getStaticFinal(l, "SI", int.class, self));
        row("boxed", () -> ConstantBootstraps.getStaticFinal(l, "BOXED", Integer.class, self));
        row("not-final", () -> ConstantBootstraps.getStaticFinal(l, "notFinal", String.class, self));
        row("missing", () -> ConstantBootstraps.getStaticFinal(l, "nope", String.class, self));
        row("three-arg", () -> ConstantBootstraps.getStaticFinal(l, "TRUE", Boolean.class));
        row("jdk", () -> ConstantBootstraps.getStaticFinal(l, "MAX_VALUE", int.class, Integer.class));
    }
}
