// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/** A secondary thread sleeps and prints; main joins. Isolates the JDK 21 hang seen in the exit probes. */
public final class Sbr5SyspropsSleep {
    public static void main(String[] a) throws Exception {
        Thread t = new Thread(() -> {
            try { Thread.sleep(300); } catch (InterruptedException e) { }
            System.out.println("slept");
        });
        t.start();
        t.join();
        System.out.println("joined");
    }
}
