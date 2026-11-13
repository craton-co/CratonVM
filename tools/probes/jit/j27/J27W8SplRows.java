// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 8 (lane W8SPL): a spin loop whose `Thread.onSpinWait()` sits inside
// a SPLICED helper (one and two levels down) still re-reads its plain flag.
// Page: docs/feature-designs/jit-ir-optimizations.md
// (the "nested splices" residual; switch CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER,
// opt-in).
//
// Rows (each a count of rounds, of j27.w8spl.spins, whose spinner thread did
// not see the release within 10 s; it stops at the first stuck round):
//   nested-field-stuck    while (!f.plain) relax();            relax() { Thread.onSpinWait(); }
//   nested-static-stuck   while (!STATIC_PLAIN) relax();
//   nested2-field-stuck   while (!f.plain) relaxTwice();       relaxTwice() { relax(); }
// HotSpot 25 (x86-64): C2 inlines relax() and intrinsifies onSpinWait as a
// MemBar over the whole memory state, so the read stays in the loop.
//
//   javac -d <dir> J27W8SplRows.java
//   java -cp <dir> J27W8SplRows > hs.txt
//   cratonvm --cp <dir> J27W8SplRows | diff - hs.txt
//     x {--real-jdk, --compatible}
//     x {default, CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER=1}
//   -Dj27.w8spl.spins=N (default 40).
//   engagement (=1 arm): CRATONVM_DBG_JITC=1 2>&1 | grep -E 'ir-on-spin-wait|on-spin-wait \(kept'
//     must show `inline-resolve REFUSED java/lang/Thread.onSpinWait()V depth=1: on-spin-wait`
//     and an `ir-on-spin-wait n<id> (PAUSE, no call)` line for the compile of
//     `J27W8SplRows.spinNestedField` / `spinNestedStatic`.
//
// Expected HotSpot 25 output:
//   fact nested-field-stuck 0
//   fact nested-static-stuck 0
//   fact nested2-field-stuck 0
//   fact done
// CratonVM --real-jdk before the fix (default arm): a `1` on at least one
// stuck row once the optimizing tier compiled the spinner with relax() spliced.

public class J27W8SplRows {
    static final int SPINS = Integer.getInteger("j27.w8spl.spins", 40);

    static final class Flag {
        boolean plain;
    }

    static boolean STATIC_PLAIN;

    static void relax() {
        Thread.onSpinWait();
    }

    static void relaxTwice() {
        relax();
    }

    static int spinNestedField(Flag f) {
        int n = 0;
        while (!f.plain) {
            relax();
            n++;
        }
        return n;
    }

    static int spinNestedStatic() {
        int n = 0;
        while (!STATIC_PLAIN) {
            relax();
            n++;
        }
        return n;
    }

    static int spinNested2Field(Flag f) {
        int n = 0;
        while (!f.plain) {
            relaxTwice();
            n++;
        }
        return n;
    }

    static void spinKind(int kind, Flag f) {
        switch (kind) {
            case 0 -> spinNestedField(f);
            case 1 -> spinNestedStatic();
            default -> spinNested2Field(f);
        }
    }

    static void release(int kind, Flag f) {
        if (kind == 1) {
            STATIC_PLAIN = true;
        } else {
            f.plain = true;
        }
    }

    static int spinRounds(int kind, int rounds) throws InterruptedException {
        for (int r = 0; r < rounds; r++) {
            Flag f = new Flag();
            STATIC_PLAIN = false;
            Thread t = new Thread(() -> spinKind(kind, f), "w8spl-spin-" + kind);
            t.setDaemon(true);
            t.start();
            // Mostly short spins; every eighth round long enough for an OSR or
            // a method compile of the spinner.
            Thread.sleep((r % 8 == 7) ? 40 : 2);
            release(kind, f);
            t.join(10_000);
            if (t.isAlive()) {
                return 1;
            }
        }
        return 0;
    }

    static void fact(String name, long value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) throws Exception {
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(300_000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("fact watchdog fired");
            System.out.flush();
            Runtime.getRuntime().halt(3);
        }, "watchdog");
        watchdog.setDaemon(true);
        watchdog.start();
        fact("nested-field-stuck", spinRounds(0, SPINS));
        fact("nested-static-stuck", spinRounds(1, SPINS));
        fact("nested2-field-stuck", spinRounds(2, SPINS));
        System.out.println("fact done");
    }
}
