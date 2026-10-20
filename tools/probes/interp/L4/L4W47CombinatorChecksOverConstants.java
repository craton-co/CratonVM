// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4: the `MethodHandles` combinator
// checks (wave 42, `L4W42CombinatorChecks`) judged a TYPE only when every
// handle involved was a direct `findStatic` / `findVirtual` handle, so a
// `constant` / `identity` / `zero` / `empty` handle (minted with its exact
// type) passed any check: `filterReturnValue(f, identity(String))` over an
// `int` result and `guardWithTest(identity(int), ..)` built adapters HotSpot
// refuses. And `dropArguments` never ran `dropArgumentChecks`: a position
// past the target's parameter count built an adapter. The drop message
// quotes the value-type array by its identity hash, masked here as `@H`.
//
// Positive control: row `filter-identity` (the base printed
// `MethodHandle(int)String` / `built`).
//
// Run: javac -d out L4W47CombinatorChecksOverConstants.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W47CombinatorChecksOverConstants
// (`--compatible` does not run the combinator checks, by design: its lines
// for the refused rows are not recorded.)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   filter-identity: java.lang.IllegalArgumentException: target and filter types do not match: (int)int, (String)String
//   filter-ok: 10
//   gwt-identity: java.lang.IllegalArgumentException: guard type is not a predicate (int)int
//   gwt-constant-ok: 6
//   gwt-identity-args: java.lang.IllegalArgumentException: target and test types must match: (int)int != (boolean)boolean
//   drop-past: java.lang.IllegalArgumentException: no argument type to remove[(int)int, 5, [Ljava.lang.Class;@H, 2, 1]
//   drop-negative: java.lang.IllegalArgumentException: no argument type to remove[(int)int, -1, [Ljava.lang.Class;@H, 3, 1]
//   drop-none-past: java.lang.IllegalArgumentException: no argument type to remove[(int)int, 3, [Ljava.lang.Class;@H, 1, 1]
//   drop-ok: 6 MethodHandle(int,String)int
//   drop-end-ok: 8 MethodHandle(int,long)int
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W47CombinatorChecksOverConstants {
    static int twice(int x) {
        return 2 * x;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            Object r = body.run();
            out = r instanceof MethodHandle ? "built" : String.valueOf(r);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle tw = MethodHandles.lookup().findStatic(L4W47CombinatorChecksOverConstants.class, "twice",
                MethodType.methodType(int.class, int.class));
        row("filter-identity", () -> MethodHandles.filterReturnValue(tw, MethodHandles.identity(String.class)));
        row("filter-ok", () -> (int) MethodHandles.filterReturnValue(tw, MethodHandles.identity(int.class)).invokeExact(5));
        row("gwt-identity", () -> MethodHandles.guardWithTest(MethodHandles.identity(int.class), tw, tw));
        row("gwt-constant-ok", () -> (int) MethodHandles.guardWithTest(
                MethodHandles.constant(boolean.class, true), tw, tw).invokeExact(3));
        row("gwt-identity-args", () -> MethodHandles.guardWithTest(
                MethodHandles.identity(boolean.class), tw, tw));
        row("drop-past", () -> MethodHandles.dropArguments(tw, 5, int.class));
        row("drop-negative", () -> MethodHandles.dropArguments(tw, -1, int.class, int.class));
        row("drop-none-past", () -> MethodHandles.dropArguments(tw, 3));
        row("drop-ok", () -> {
            MethodHandle d = MethodHandles.dropArguments(tw, 1, String.class);
            return (int) d.invokeExact(3, "x") + " " + d;
        });
        row("drop-end-ok", () -> {
            MethodHandle d = MethodHandles.dropArguments(tw, 1, long.class);
            return (int) d.invokeExact(4, 9L) + " " + d;
        });
    }
}
