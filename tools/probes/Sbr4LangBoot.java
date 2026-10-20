// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleProxies;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;

/**
 * SBR round 4, lane {@code lang}: the FIRST reflective use in the process, one
 * entry point per run (the reflection-access hook is published by
 * {@code AccessibleObject.<clinit>}; HotSpot has run it before {@code main}).
 * A VM that left it uninitialised answers {@code NullPointerException} or a
 * null {@code langReflectAccess} from the first use.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr4LangBoot newInstance|proxy|setAccessible|ctor</pre>
 */
public final class Sbr4LangBoot {
    public static class Box {
        public Box() {}
        @Override public String toString() { return "Box"; }
    }

    static String run() { return "ran"; }

    public static void main(String[] a) throws Throwable {
        String row = a.length > 0 ? a[0] : "newInstance";
        switch (row) {
            case "newInstance":
                System.out.println("newInstance " + Box.class.newInstance());
                break;
            case "proxy": {
                MethodHandle mh = MethodHandles.lookup()
                        .findStatic(Sbr4LangBoot.class, "run", MethodType.methodType(String.class));
                java.util.function.Supplier<String> s =
                        MethodHandleProxies.asInterfaceInstance(java.util.function.Supplier.class, mh);
                System.out.println("proxy " + s.get());
                break;
            }
            case "setAccessible": {
                Constructor<Box> c = Box.class.getDeclaredConstructor();
                c.setAccessible(true);
                System.out.println("setAccessible " + c.newInstance());
                break;
            }
            case "ctor": {
                Constructor<Box> c = Box.class.getConstructor();
                System.out.println("ctor " + c.newInstance() + " " + c.getParameterCount());
                break;
            }
            default:
                System.out.println("unknown row " + row);
        }
    }
}
