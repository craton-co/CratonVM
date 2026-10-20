// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L1: a thread blocked in `Thread.join`
// reads `WAITING` (untimed join) or `TIMED_WAITING` (timed join) for the whole
// join, as in HotSpot (`join` is `wait(0)` / `wait(delay)` on the target).
// CratonVM's served joins poll the target's liveness in 2 ms slices and left
// their blocking region between two slices to read the target's `Thread`
// object, so for a moment in every slice the joiner read `RUNNABLE` (and JDWP
// `ThreadReference.Status` read RUNNING; `interpreter-L1-a-served-timed-join-reads-runnable-between-its-slices-FIXED`).
// Wave 50 polls the target's registry id, which reads no heap, and keeps one
// region for the whole join.
//
// Rows: each joiner is sampled with `getState()` as fast as the main thread
// can for ~800 ms once it is blocked; the row prints whether every sample
// read the one expected state.
//
// HotSpot 25 prints (`java`, default flags; measured, JDK 25.0.3):
//     untimed-join only=WAITING true
//     timed-join only=TIMED_WAITING true
//     done
// CratonVM before wave 50: `false` on one or both rows (a RUNNABLE sample),
// most runs. `--compatible` and `--jdk-only` alike.
//
//   javac -d out L1W50JoinStateStaysWaiting.java
//   cratonvm --java-home <jdk25> [--compatible] [--nojit] -cp out L1W50JoinStateStaysWaiting
public class L1W50JoinStateStaysWaiting {
    static void row(String label, boolean timed) throws Exception {
        Object gate = new Object();
        Thread target = new Thread(() -> {
            synchronized (gate) {
                try {
                    gate.wait();
                } catch (InterruptedException e) {
                    // released below
                }
            }
        }, "target-" + label);
        target.setDaemon(true);
        target.start();
        Thread joiner = new Thread(() -> {
            try {
                if (timed) {
                    target.join(60_000);
                } else {
                    target.join();
                }
            } catch (InterruptedException e) {
                // ends the row
            }
        }, "joiner-" + label);
        joiner.setDaemon(true);
        joiner.start();
        Thread.State expected = timed ? Thread.State.TIMED_WAITING : Thread.State.WAITING;
        long until = System.nanoTime() + 5_000_000_000L;
        while (joiner.getState() != expected && System.nanoTime() < until) {
            Thread.onSpinWait();
        }
        boolean only = joiner.getState() == expected;
        long end = System.nanoTime() + 800_000_000L;
        long samples = 0;
        while (System.nanoTime() < end) {
            if (joiner.getState() != expected) {
                only = false;
            }
            samples++;
        }
        target.interrupt();
        joiner.join(5_000);
        System.out.println(label + " only=" + expected + " " + (only && samples > 0));
    }

    public static void main(String[] args) throws Exception {
        row("untimed-join", false);
        row("timed-join", true);
        System.out.println("done");
    }
}
