// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ServiceLoader;

/**
 * SBR2 wave 7, lane MODULE: what the FIRST touch of the module graph costs in a fresh process
 * (the boot layer is built and, now, its reads/exports/opens tables filled). One number per line, in
 * microseconds: the first {@code Class.getModule()} of a boot class, the first {@code
 * ModuleLayer.boot()}, the first {@code ServiceLoader} iteration, a {@code Class.forName} loop and a
 * class-loading loop over the application loader (Spring/Tomcat shape). Run it in a FRESH process
 * per rep, alternate the binaries, take medians.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr2W7ModuleStart
 * </pre>
 */
public class Sbr2W7ModuleStart {
    static long us(long t0) {
        return (System.nanoTime() - t0) / 1000;
    }

    public interface Svc {}

    public static void main(String[] a) throws Throwable {
        long t0 = System.nanoTime();
        Module m = String.class.getModule();
        System.out.println("first getModule " + us(t0));
        t0 = System.nanoTime();
        ModuleLayer.boot();
        System.out.println("first boot() " + us(t0));
        t0 = System.nanoTime();
        boolean r = m.isExported("java.lang");
        System.out.println("first isExported " + us(t0) + " " + r);
        t0 = System.nanoTime();
        int n = 0;
        for (java.nio.charset.spi.CharsetProvider p : ServiceLoader.load(java.nio.charset.spi.CharsetProvider.class)) {
            n++;
        }
        System.out.println("ServiceLoader " + us(t0) + " " + n);
        t0 = System.nanoTime();
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        String[] names = {"java.util.ArrayList", "java.util.HashMap", "java.util.logging.Logger", "java.sql.Date",
            "java.util.concurrent.ConcurrentHashMap", "java.lang.invoke.MethodHandles", "java.util.regex.Pattern",
            "java.text.SimpleDateFormat", "java.net.URI", "java.util.zip.ZipFile"};
        int k = 0;
        for (int i = 0; i < 20000; i++) {
            Class<?> c = Class.forName(names[i % names.length], false, sys);
            k += c.getModule().isExported(c.getPackageName()) ? 1 : 0;
            k += Object.class.getModule().canRead(c.getModule()) ? 1 : 0;
        }
        System.out.println("forName+module loop " + us(t0) + " " + k);
        t0 = System.nanoTime();
        for (int i = 0; i < 20000; i++) {
            k += String.class.getModule() == Object.class.getModule() ? 1 : 0;
            k += sys.getUnnamedModule().canRead(Object.class.getModule()) ? 1 : 0;
        }
        System.out.println("getModule loop " + us(t0) + " " + k);
    }
}
