// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;
import java.util.Properties;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * SBR round 3 wave 2, lane {@code refmisc}: {@code System.setProperties(null)} installs a fresh
 * {@code Properties} of the boot properties, as HotSpot's does, and survives collections (the
 * native builds the object across several Java calls). One thread swaps and checks while others
 * churn the heap; every fresh object must hold the boot-time {@code java.version} and
 * {@code file.separator}, must not be the previous object, and must not hold a property set
 * after boot.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx64m [-XX:+UseG1GC | -XX:+UseZGC] -cp . Sbr3RefmiscSetPropsGc
 *   java -Xmx64m -cp . Sbr3RefmiscSetPropsGc                      # HotSpot
 * </pre>
 */
public final class Sbr3RefmiscSetPropsGc {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String version = System.getProperty("java.version");
        String sep = System.getProperty("file.separator");
        Properties original = System.getProperties();
        AtomicBoolean stop = new AtomicBoolean();
        Thread[] churn = new Thread[3];
        for (int i = 0; i < churn.length; i++) {
            churn[i] = new Thread(() -> {
                List<byte[]> junk = new ArrayList<>();
                while (!stop.get()) {
                    junk.add(new byte[4096]);
                    if (junk.size() > 2000) {
                        junk.clear();
                    }
                }
            });
            churn[i].setDaemon(true);
            churn[i].start();
        }
        int bad = 0;
        int rounds = 0;
        long until = System.nanoTime() + 8_000_000_000L;
        while (System.nanoTime() < until) {
            rounds++;
            System.setProperty("sbr3.after.boot", "x" + rounds);
            Properties prev = System.getProperties();
            System.setProperties(null);
            Properties now = System.getProperties();
            boolean ok = now != prev
                    && version.equals(now.getProperty("java.version"))
                    && sep.equals(now.getProperty("file.separator"))
                    && now.getProperty("sbr3.after.boot") == null
                    && version.equals(System.getProperty("java.version"))
                    && System.getProperty("sbr3.after.boot") == null;
            if (!ok) {
                bad++;
            }
            sink = now;
        }
        System.setProperties(original);
        stop.set(true);
        System.out.println("rounds>0=" + (rounds > 0) + " bad=" + bad);
        System.out.println(bad == 0 ? "SETPROPS-GC-OK" : "SETPROPS-GC-FAIL");
    }
}
