// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * SBR2 wave 7, lane MODULE: what the command-line module edges look like to the JDK's own
 * {@code Module} predicates. Run it with and without
 * {@code --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --add-opens java.base/java.lang=ALL-UNNAMED
 * --add-opens java.base/java.util=java.logging --add-reads java.logging=ALL-UNNAMED}
 * and diff against HotSpot.
 */
public class Sbr2W7ModuleFlags {
    static String tf(boolean b) {
        return b ? "1" : "0";
    }

    public static void main(String[] args) throws Throwable {
        Module self = Sbr2W7ModuleFlags.class.getModule();
        Module base = Object.class.getModule();
        Module logging = java.util.logging.Logger.class.getModule();
        System.out.println("base.isExported(jdk.internal.misc,self) " + tf(base.isExported("jdk.internal.misc", self)));
        System.out.println("base.isExported(jdk.internal.misc) " + tf(base.isExported("jdk.internal.misc")));
        System.out.println("base.isOpen(java.lang,self) " + tf(base.isOpen("java.lang", self)));
        System.out.println("base.isOpen(java.lang) " + tf(base.isOpen("java.lang")));
        System.out.println("base.isOpen(java.util,logging) " + tf(base.isOpen("java.util", logging)));
        System.out.println("base.isOpen(java.util,self) " + tf(base.isOpen("java.util", self)));
        System.out.println("base.isOpen(java.io,self) " + tf(base.isOpen("java.io", self)));
        System.out.println("logging.canRead(self) " + tf(logging.canRead(self)));
        try {
            Field f = String.class.getDeclaredField("value");
            f.setAccessible(true);
            System.out.println("String.value setAccessible ok");
        } catch (Throwable t) {
            System.out.println("String.value " + t.getClass().getName());
        }
        try {
            Field f = Class.forName("java.io.File").getDeclaredField("path");
            f.setAccessible(true);
            System.out.println("File.path setAccessible ok");
        } catch (Throwable t) {
            System.out.println("File.path " + t.getClass().getName());
        }
    }
}
