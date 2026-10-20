// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.util.Set;
import java.util.TreeSet;

/**
 * SBR round 3, lane {@code ref}: what {@code StackTraceElement.HashedModules} holds. Needs
 * {@code --add-opens java.base/java.lang=ALL-UNNAMED}.
 */
public final class Sbr3RefHashDiag {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("java.lang.StackTraceElement$HashedModules");
        Field f = c.getDeclaredField("HASHED_MODULES");
        f.setAccessible(true);
        Set<?> s = (Set<?>) f.get(null);
        TreeSet<String> names = new TreeSet<>();
        for (Object m : s) {
            names.add(((Module) m).getName());
        }
        System.out.println("hashed modules=" + names.size() + " " + names);
        StackTraceElement e = new Throwable().getStackTrace()[0];
        System.out.println(e.getModuleName() + " " + e.getModuleVersion());
        try {
            java.util.List.of().get(1);
        } catch (Throwable t) {
            System.out.println(t.getStackTrace()[0].toString().replaceAll(":\\d+\\)", ":N)"));
        }
    }
}
