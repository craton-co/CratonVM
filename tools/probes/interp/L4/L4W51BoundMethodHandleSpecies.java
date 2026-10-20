// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L4: `java.lang.invoke.BoundMethodHandle`
// and its pregenerated species initialize (page
// `docs/internal/fixed-bugs/interpreter-L4-bound-method-handle-species-do-not-link-FIXED-20261015.md`).
// Wave 49: `BoundMethodHandle.<clinit>` failed linking `SimpleMethodHandle`'s
// species (a null static-field base); wave 50: it failed with
// `IllegalAccessError: failed to access class
// java.lang.invoke.BoundMethodHandle$Species_L from class
// java.lang.invoke.BoundMethodHandle`, and every later use of the class was
// `NoClassDefFoundError: Could not initialize class`. The first row runs on a
// cold VM; `init-bmh-again` repeats it.
//
// Run: javac -d out L4W51BoundMethodHandleSpecies.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W51BoundMethodHandleSpecies
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   init-bmh: java.lang.invoke.BoundMethodHandle
//   species-l: java.lang.invoke.BoundMethodHandle$Species_L private static final
//   simple: java.lang.invoke.SimpleMethodHandle
//   bind-to: ab
//   insert: xyz
//   init-bmh-again: java.lang.invoke.BoundMethodHandle
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;

public class L4W51BoundMethodHandleSpecies {
    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
            Throwable c = t.getCause();
            if (c != null) {
                out += " / cause " + c.getClass().getName() + ": " + c.getMessage();
            }
        }
        System.out.println(name + ": " + out);
    }

    static Object init(String name) throws Throwable {
        return Class.forName(name, true, null).getName();
    }

    public static void main(String[] args) {
        row("init-bmh", () -> init("java.lang.invoke.BoundMethodHandle"));
        row("species-l", () -> {
            Class<?> c = Class.forName("java.lang.invoke.BoundMethodHandle$Species_L", true, null);
            return c.getName() + " " + Modifier.toString(c.getModifiers());
        });
        row("simple", () -> init("java.lang.invoke.SimpleMethodHandle"));
        row("bind-to", () -> {
            MethodHandle concat = MethodHandles.lookup().findVirtual(String.class, "concat",
                MethodType.methodType(String.class, String.class));
            return (String) concat.bindTo("a").invokeExact("b");
        });
        row("insert", () -> {
            MethodHandle concat = MethodHandles.lookup().findVirtual(String.class, "concat",
                MethodType.methodType(String.class, String.class));
            MethodHandle h = MethodHandles.insertArguments(concat, 1, "yz");
            return (String) h.invokeExact("x");
        });
        row("init-bmh-again", () -> init("java.lang.invoke.BoundMethodHandle"));
    }
}
