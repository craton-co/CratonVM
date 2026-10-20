// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.locks.LockSupport;

/**
 * SBR2 wave 10: parity probe for the two places the dev merge made a judgment call
 * (`Class.newInstance` special cases, `Thread.interrupt` with a subclass that declares
 * its own `interrupted` field). Diff the output against HotSpot, in both modes.
 */
public class Sbr2W10DevMergeParity {
    static class W extends Thread { volatile boolean interrupted; }
    static class NoCtor { NoCtor(int x) {} }
    static abstract class Abs {}
    interface I {}

    static void ni(String tag, Class<?> c) {
        try { Object o = c.newInstance(); System.out.println(tag + " ok " + (o == null ? "null" : o.getClass().getName())); }
        catch (Throwable t) { System.out.println(tag + " " + t.getClass().getName() + " | " + t.getMessage()); }
    }

    public static void main(String[] a) throws Exception {
        ni("Class.class", Class.class);
        ni("int", int.class);
        ni("void", void.class);
        ni("int[]", int[].class);
        ni("abstract", Abs.class);
        ni("interface", I.class);
        ni("noctor", NoCtor.class);
        ni("Object", Object.class);
        ni("String", String.class);

        W w = new W();
        w.start();
        Thread.sleep(50);
        w.interrupt();
        w.join(2000);
        System.out.println("sub.isInterrupted-after-join alive=" + w.isAlive());

        W p = new W() { public void run() { LockSupport.park(); } };
        p.start();
        Thread.sleep(100);
        p.interrupt();
        System.out.println("sub.isInterrupted=" + p.isInterrupted());
        p.join(3000);
        System.out.println("parked subclass thread woke: alive=" + p.isAlive());

        Thread t = new Thread(() -> { try { Thread.sleep(60000); } catch (InterruptedException e) { System.out.println("plain woke: " + Thread.currentThread().isInterrupted()); } });
        t.start(); Thread.sleep(100); t.interrupt(); t.join(3000);
        System.out.println("plain alive=" + t.isAlive());
        Thread.currentThread().interrupt();
        System.out.println("self=" + Thread.interrupted() + "," + Thread.interrupted());
    }
}
