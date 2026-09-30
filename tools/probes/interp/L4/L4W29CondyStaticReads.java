// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: `CONSTANT_Dynamic` entries
// bootstrapped by `ConstantBootstraps.getStaticFinal` / `enumConstant`.
// CratonVM answers some of them without running the bootstrap
// (`constants.rs::condy_fast_answer` / `condy_public_static_reference`); that
// is sound only where the answer equals the bootstrap's, failures included:
//
//   getStaticFinal of a non-final field   IncompatibleClassChangeError "not a
//                                         final field: <name>", and the owner
//                                         is NOT initialized (no H.<clinit>
//                                         before the finalField row)
//   the 4-argument getStaticFinal with no BootstrapMethodError around a
//   static argument                       WrongMethodTypeException
//   enumConstant of a static field that   BootstrapMethodError around
//   is not an enum constant, or of a      Enum.valueOf's IllegalArgumentException
//   class that is not an enum
//
// Before wave 29 the shortcut matched on the bootstrap's NAME only, never
// checked ACC_FINAL / ACC_ENUM or the owner's accessibility, and initialized
// the owner first: `nonFinalField` returned "nonfinal", `nonFinalSelfTyped`
// "H.SELF", `fourArgNoDeclaringClass` "H.FSELF", `enumAlias` GREEN,
// `notAnEnum` "H.FSELF", `booleanNotAnEnum` true, and "H.<clinit>" printed
// first; each answer was then recorded for the entry (every mode). Reported
// by two wave-29 L4 review passes. `L4W29CondyUser` is defined in this
// class's own loader and package (`Lookup.defineClass`), so the owners are
// accessible and the shortcut is consulted; the final / enum rows are the
// shortcut's positive controls.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W29CondyStaticReads
// (the same lines in every mode)
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   nonFinalField #0: java.lang.IncompatibleClassChangeError: not a final field: NF
//   nonFinalField #1: java.lang.IncompatibleClassChangeError: not a final field: NF
//   nonFinalSelfTyped #0: java.lang.IncompatibleClassChangeError: not a final field: SELF
//   nonFinalSelfTyped #1: java.lang.IncompatibleClassChangeError: not a final field: SELF
//   fourArgNoDeclaringClass #0: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,Class,Class)Object to (Lookup,String,Class)Object
//   fourArgNoDeclaringClass #1: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Lookup,String,Class,Class)Object to (Lookup,String,Class)Object
//   H.<clinit>
//   finalField #0: returned final-x
//   finalField #1: returned final-x
//   finalSelfTyped #0: returned H.FSELF
//   finalSelfTyped #1: returned H.FSELF
//   enumConst #0: returned RED
//   enumConst #1: returned RED
//   enumAlias #0: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: No enum constant L4W29CondyStaticReads.Color.DEFAULT
//   enumAlias #1: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: No enum constant L4W29CondyStaticReads.Color.DEFAULT
//   notAnEnum #0: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: L4W29CondyStaticReads$H is not an enum class
//   notAnEnum #1: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: L4W29CondyStaticReads$H is not an enum class
//   booleanNotAnEnum #0: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: java.lang.Boolean is not an enum class
//   booleanNotAnEnum #1: java.lang.BootstrapMethodError: bootstrap method initialization exception cause java.lang.IllegalArgumentException: java.lang.Boolean is not an enum class
//
// `USER` was built with java.lang.classfile; each method is `ldc
// DynamicConstantDesc.ofNamed(bsm, name, type[, declaring])` + `areturn`
// (gsf4 / gsf3 = the 4- / 3-argument `getStaticFinal`, ec = `enumConstant`):
//   finalField gsf4 FS String H    finalSelfTyped gsf3 FSELF H
//   nonFinalField gsf4 NF String H nonFinalSelfTyped gsf3 SELF H
//   fourArgNoDeclaringClass gsf4 FSELF H (no static argument)
//   enumConst ec RED Color  enumAlias ec DEFAULT Color  notAnEnum ec FSELF H
//   booleanNotAnEnum ec TRUE java.lang.Boolean

import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.util.Base64;

public class L4W29CondyStaticReads {
    public static class H {
        static { System.out.println("H.<clinit>"); }
        public static final String FS = "final-" + "x".repeat(1);
        public static final H FSELF = new H("H.FSELF");
        public static String NF = "nonfinal";
        public static H SELF = new H("H.SELF");
        private final String s;
        H(String s) { this.s = s; }
        public String toString() { return s; }
    }

    public enum Color {
        RED, GREEN;
        public static final Color DEFAULT = GREEN;
    }

    // `L4W29CondyUser`, generated with java.lang.classfile: each public static
    // method is one `ldc` of a CONSTANT_Dynamic and an `areturn`.
    static final String USER =
        "yv66vgAAAEUAPgEADkw0VzI5Q29uZHlVc2VyBwABAQAKZmluYWxGaWVsZAEAFCgpTGphdmEvbGFuZy9PYmplY3Q7" +
        "AQAjamF2YS9sYW5nL2ludm9rZS9Db25zdGFudEJvb3RzdHJhcHMHAAUBAA5nZXRTdGF0aWNGaW5hbAEAbyhMamF2" +
        "YS9sYW5nL2ludm9rZS9NZXRob2RIYW5kbGVzJExvb2t1cDtMamF2YS9sYW5nL1N0cmluZztMamF2YS9sYW5nL0Ns" +
        "YXNzO0xqYXZhL2xhbmcvQ2xhc3M7KUxqYXZhL2xhbmcvT2JqZWN0OwwABwAICgAGAAkPBgAKAQAXTDRXMjlDb25k" +
        "eVN0YXRpY1JlYWRzJEgHAAwBAAJGUwEAEkxqYXZhL2xhbmcvU3RyaW5nOwwADgAPEQAAABABAA5maW5hbFNlbGZU" +
        "eXBlZAEAXihMamF2YS9sYW5nL2ludm9rZS9NZXRob2RIYW5kbGVzJExvb2t1cDtMamF2YS9sYW5nL1N0cmluZztM" +
        "amF2YS9sYW5nL0NsYXNzOylMamF2YS9sYW5nL09iamVjdDsMAAcAEwoABgAUDwYAFQEABUZTRUxGAQAZTEw0VzI5" +
        "Q29uZHlTdGF0aWNSZWFkcyRIOwwAFwAYEQABABkBAA1ub25GaW5hbEZpZWxkAQACTkYMABwADxEAAAAdAQARbm9u" +
        "RmluYWxTZWxmVHlwZWQBAARTRUxGDAAgABgRAAEAIQEAF2ZvdXJBcmdOb0RlY2xhcmluZ0NsYXNzEQACABkBAAll" +
        "bnVtQ29uc3QBAAxlbnVtQ29uc3RhbnQBAFwoTGphdmEvbGFuZy9pbnZva2UvTWV0aG9kSGFuZGxlcyRMb29rdXA7" +
        "TGphdmEvbGFuZy9TdHJpbmc7TGphdmEvbGFuZy9DbGFzczspTGphdmEvbGFuZy9FbnVtOwwAJgAnCgAGACgPBgAp" +
        "AQADUkVEAQAdTEw0VzI5Q29uZHlTdGF0aWNSZWFkcyRDb2xvcjsMACsALBEAAwAtAQAJZW51bUFsaWFzAQAHREVG" +
        "QVVMVAwAMAAsEQADADEBAAlub3RBbkVudW0RAAMAGQEAEGJvb2xlYW5Ob3RBbkVudW0BAARUUlVFAQATTGphdmEv" +
        "bGFuZy9Cb29sZWFuOwwANgA3EQADADgBABBqYXZhL2xhbmcvT2JqZWN0BwA6AQAEQ29kZQEAEEJvb3RzdHJhcE1l" +
        "dGhvZHMAIQACADsAAAAAAAkACQADAAQAAQA8AAAADwABAAAAAAADEhGwAAAAAAAJABIABAABADwAAAAPAAEAAAAA" +
        "AAMSGrAAAAAAAAkAGwAEAAEAPAAAAA8AAQAAAAAAAxIesAAAAAAACQAfAAQAAQA8AAAADwABAAAAAAADEiKwAAAA" +
        "AAAJACMABAABADwAAAAPAAEAAAAAAAMSJLAAAAAAAAkAJQAEAAEAPAAAAA8AAQAAAAAAAxIusAAAAAAACQAvAAQA" +
        "AQA8AAAADwABAAAAAAADEjKwAAAAAAAJADMABAABADwAAAAPAAEAAAAAAAMSNLAAAAAAAAkANQAEAAEAPAAAAA8A" +
        "AQAAAAAAAxI5sAAAAAAAAQA9AAAAFAAEAAsAAQANABYAAAALAAAAKgAA";

    public static void main(String[] a) throws Throwable {
        // Defined in this class's own loader and package, so the owners are
        // accessible and the VM's shortcut for these reads is consulted.
        Class<?> user = MethodHandles.lookup().defineClass(Base64.getDecoder().decode(USER));
        for (String m : new String[] {"nonFinalField", "nonFinalSelfTyped", "fourArgNoDeclaringClass",
                                      "finalField", "finalSelfTyped", "enumConst", "enumAlias",
                                      "notAnEnum", "booleanNotAnEnum"}) {
            for (int i = 0; i < 2; i++) {
                String r;
                try {
                    r = "returned " + user.getMethod(m).invoke(null);
                } catch (InvocationTargetException e) {
                    Throwable t = e.getCause();
                    r = t.toString() + (t.getCause() == null ? "" : " cause " + t.getCause());
                }
                System.out.println(m + " #" + i + ": " + r);
            }
        }
    }
}
