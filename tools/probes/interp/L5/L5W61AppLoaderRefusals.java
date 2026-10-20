// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L5 -- a class file the application
// loader FINDS but the definer refuses is the definer's error, never "not
// found" (`i60-L5-proposal-the-resolver-reports-absence-as-its-own-shape`).
// Wave 60 named three refusals (wrong name, bad magic, newer version); these
// are two it did not: a class whose superclass is an interface
// (`IncompatibleClassChangeError`) and a superclass cycle
// (`ClassCircularityError`). A missing name stays `ClassNotFoundException`.
//
// Self-contained: on start the probe rewrites two of its own nested class
// files IN ITS CLASS-PATH DIRECTORY (same-length name patches, idempotent):
// `SubA`'s superclass `BaseA` becomes the interface `IfceA`, and `BaseC`'s
// superclass `Zzzz` becomes `SubC` (which extends `BaseC`). Neither class is
// loaded before. The first row is repeated last (cold first call).
//
// Run: javac -d out L5W61AppLoaderRefusals.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W61AppLoaderRefusals
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   iface-super forName=java.lang.IncompatibleClassChangeError: class L5W61AppLoaderRefusals$SubA has interface L5W61AppLoaderRefusals$IfceA as super class
//   iface-super loadClass=java.lang.IncompatibleClassChangeError: class L5W61AppLoaderRefusals$SubA has interface L5W61AppLoaderRefusals$IfceA as super class
//   circularity forName=java.lang.ClassCircularityError: L5W61AppLoaderRefusals$SubC
//   circularity loadClass=java.lang.ClassCircularityError: L5W61AppLoaderRefusals$SubC
//   missing forName=java.lang.ClassNotFoundException: L5W61AppLoaderRefusals$Nope
//   present forName=L5W61AppLoaderRefusals$Zzzz
//   iface-super again=java.lang.IncompatibleClassChangeError: class L5W61AppLoaderRefusals$SubA has interface L5W61AppLoaderRefusals$IfceA as super class

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.Callable;

public class L5W61AppLoaderRefusals {
    interface IfceA {}
    static class BaseA {}
    static class SubA extends BaseA {}

    static class Zzzz {}
    static class BaseC extends Zzzz {}
    static class SubC extends BaseC {}

    static final String OUTER = "L5W61AppLoaderRefusals$";

    static void patch(Path dir, String simple, String from, String to) throws Exception {
        Path file = dir.resolve(OUTER + simple + ".class");
        byte[] b = Files.readAllBytes(file);
        byte[] f = (OUTER + from).getBytes(StandardCharsets.UTF_8);
        byte[] t = (OUTER + to).getBytes(StandardCharsets.UTF_8);
        outer:
        for (int i = 0; i + f.length <= b.length; i++) {
            for (int j = 0; j < f.length; j++) {
                if (b[i + j] != f[j]) continue outer;
            }
            System.arraycopy(t, 0, b, i, t.length);
        }
        Files.write(file, b);
    }

    static String attempt(Callable<Class<?>> c) {
        try {
            return c.call().getName();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Path.of(L5W61AppLoaderRefusals.class.getProtectionDomain()
                .getCodeSource().getLocation().toURI());
        patch(dir, "SubA", "BaseA", "IfceA");
        patch(dir, "BaseC", "Zzzz", "SubC");
        ClassLoader app = ClassLoader.getSystemClassLoader();
        String a = OUTER + "SubA";
        String c = OUTER + "SubC";
        System.out.println("iface-super forName=" + attempt(() -> Class.forName(a, false, app)));
        System.out.println("iface-super loadClass=" + attempt(() -> app.loadClass(a)));
        System.out.println("circularity forName=" + attempt(() -> Class.forName(c, false, app)));
        System.out.println("circularity loadClass=" + attempt(() -> app.loadClass(c)));
        System.out.println("missing forName="
                + attempt(() -> Class.forName(OUTER + "Nope", false, app)));
        System.out.println("present forName="
                + attempt(() -> Class.forName(OUTER + "Zzzz", false, app)));
        System.out.println("iface-super again=" + attempt(() -> Class.forName(a, false, app)));
    }
}
