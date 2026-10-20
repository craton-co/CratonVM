// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * A constant-trip counted loop whose trapping load is LOOP-VARIANT, so a deopt
 * can come out of copy k &gt; 0.
 *
 * The shape {@link UnrollFrames#invariant} cannot reach: there, every copy loads
 * the same receiver, so if any copy traps, copy 0 traps first and the later
 * copies' frames are correct but unreached. Here the walk advances, so a list
 * SHORTER than the trip count NPEs inside a specific copy — and the interpreter
 * then has to rebuild that copy's frame to resume.
 *
 * Observable either way: a resume from a wrong frame does not crash, it carries
 * on with another iteration's {@code o} and reports a different {@code sum} or a
 * different {@code traps} than HotSpot does.
 */
public class UnrollTrap {
    static final class N {
        int v;
        N next;
        N(int v, N next) { this.v = v; this.next = next; }
    }

    static int walk(N o) {
        int a = 0;
        for (int i = 0; i < 5; i++) { a += o.v; o = o.next; }
        return a;
    }

    public static void main(String[] args) {
        int reps = Integer.getInteger("probe.reps", 50000);
        N five = null;
        for (int i = 5; i > 0; i--) five = new N(i, five);
        N three = null;
        for (int i = 3; i > 0; i--) three = new N(i, three);

        long sum = 0;
        int traps = 0;
        for (int r = 0; r < reps; r++) {
            sum += walk(five);
            try {
                sum += walk(three);
            } catch (NullPointerException e) {
                traps++;
            }
        }
        System.out.println("UnrollTrap sum=" + sum + " traps=" + traps);
    }
}
