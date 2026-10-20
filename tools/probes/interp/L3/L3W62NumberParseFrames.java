// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L3 --
// docs/internal/fixed-bugs/interpreter-i62-L3-integer-parse-int-nfe-lists-no-jdk-frames-FIXED-20261111.md
// (OPEN): `Integer.parseInt` / `Long.parseLong` are served by natives (and an
// interpreter intrinsic) in both modes; their `NumberFormatException` lists
// none of the JDK frames HotSpot lists (`NumberFormatException.forInputString`
// and the `parse*` overloads), directly, reflectively or through a method
// handle. Rows print the message and frames up to the probe's own lambda, no
// lines. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// direct-parseInt: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.parseInt | L3W62NumberParseFrames.lambda$main$0
// direct-parseInt-empty: java.lang.NumberFormatException For input string: "" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.parseInt | L3W62NumberParseFrames.lambda$main$1
// direct-parseInt-radix: java.lang.NumberFormatException For input string: "x" under radix 16 java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | L3W62NumberParseFrames.lambda$main$2
// direct-parseLong: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Long.parseLong | java.lang.Long.parseLong | L3W62NumberParseFrames.lambda$main$3
// direct-valueOf: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.valueOf | L3W62NumberParseFrames.lambda$main$4
// handle-parseInt: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.parseInt | L3W62NumberParseFrames.lambda$main$5
// reflect-parseInt: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.parseInt | jdk.internal.reflect.DirectMethodHandleAccessor.invoke | java.lang.reflect.Method.invoke | L3W62NumberParseFrames.lambda$main$6
// direct-parseInt-again: java.lang.NumberFormatException For input string: "x" java.lang.NumberFormatException.forInputString | java.lang.Integer.parseInt | java.lang.Integer.parseInt | L3W62NumberParseFrames.lambda$main$7
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W62NumberParseFrames`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;

public class L3W62NumberParseFrames {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
            StringBuilder b = new StringBuilder(name + ": " + c.getClass().getName() + " " + c.getMessage() + " ");
            String sep = "";
            for (StackTraceElement e : c.getStackTrace()) {
                b.append(sep).append(e.getClassName()).append('.').append(e.getMethodName())
                        .append(e.isNativeMethod() ? "(N)" : "");
                sep = " | ";
                if (e.getMethodName().startsWith("lambda$")) {
                    break;
                }
            }
            System.out.println(b);
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle h = MethodHandles.lookup().findStatic(Integer.class, "parseInt",
                MethodType.methodType(int.class, String.class));
        row("direct-parseInt", () -> Integer.parseInt("x"));
        row("direct-parseInt-empty", () -> Integer.parseInt(""));
        row("direct-parseInt-radix", () -> Integer.parseInt("x", 16));
        row("direct-parseLong", () -> Long.parseLong("x"));
        row("direct-valueOf", () -> Integer.valueOf("x"));
        row("handle-parseInt", () -> (int) h.invokeExact("x"));
        row("reflect-parseInt", () -> Integer.class.getMethod("parseInt", String.class).invoke(null, "x"));
        row("direct-parseInt-again", () -> Integer.parseInt("x"));
    }
}
