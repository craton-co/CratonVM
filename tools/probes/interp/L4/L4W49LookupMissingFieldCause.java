// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L4 (page
// `interpreter-L4-constantbootstraps-getstaticfinal-reads-no-membername-FIXED-20261013`):
// a `Lookup` field finder of a missing field (or one of another type) raises
// `NoSuchFieldException: no such field: <member>` CAUSED by the resolution's
// `NoSuchFieldError: Class <C> does not have member field '<T> <name>'`
// (`MemberName.makeAccessException`). `ConstantBootstraps` and
// `MethodHandleNatives.mapLookupExceptionToError` surface that cause.
// CratonVM raised the exception without a cause.
//
// Run: javac -d out L4W49LookupMissingFieldCause.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W49LookupMissingFieldCause
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   static-string: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.nope/java.lang.String/getStatic / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'java.lang.String nope'
//   static-int-array: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.nope/[I/getStatic / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'int[] nope'
//   getter-2d: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.nope/[[Ljava.lang.String;/getField / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'java.lang.String[][] nope'
//   static-setter-nested: java.lang.NoSuchFieldException: no such field: java.util.AbstractMap$SimpleEntry.nope/long/putStatic / cause=java.lang.NoSuchFieldError: Class java.util.AbstractMap$SimpleEntry does not have member field 'long nope'
//   var-handle: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.nope/int/getField / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'int nope'
//   wrong-type: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.SF/java.lang.Object/getStatic / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'java.lang.Object SF'
//   get-static-final-missing: java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'java.lang.String nope'
//   get-static-final: sf
//   static-string-again: java.lang.NoSuchFieldException: no such field: L4W49LookupMissingFieldCause.nope/java.lang.String/getStatic / cause=java.lang.NoSuchFieldError: Class L4W49LookupMissingFieldCause does not have member field 'java.lang.String nope'
import java.lang.invoke.ConstantBootstraps;
import java.lang.invoke.MethodHandles;

public class L4W49LookupMissingFieldCause {
    static final String SF = "sf";

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t + (t.getCause() == null ? "" : " / cause=" + t.getCause());
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> self = L4W49LookupMissingFieldCause.class;
        row("static-string", () -> l.findStaticGetter(self, "nope", String.class));
        row("static-int-array", () -> l.findStaticGetter(self, "nope", int[].class));
        row("getter-2d", () -> l.findGetter(self, "nope", String[][].class));
        row("static-setter-nested", () -> l.findStaticSetter(java.util.AbstractMap.SimpleEntry.class, "nope", long.class));
        row("var-handle", () -> l.findVarHandle(self, "nope", int.class));
        row("wrong-type", () -> l.findStaticGetter(self, "SF", Object.class));
        row("get-static-final-missing", () -> ConstantBootstraps.getStaticFinal(l, "nope", String.class, self));
        row("get-static-final", () -> ConstantBootstraps.getStaticFinal(l, "SF", String.class, self));
        row("static-string-again", () -> l.findStaticGetter(self, "nope", String.class));
    }
}
