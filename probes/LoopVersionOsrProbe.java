// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.regex.Matcher;
import java.util.regex.Pattern;

// Regression reproducer for the wrong-code failure guarded loop versioning
// shipped with, and the bisect that localised it.
//
//   CRATONVM_JIT='bytecode-loop-xform,deopt-real=0' \
//     cratonvm --java-home <jdk> -cp probes LoopVersionOsrProbe
//
// All three methods must print the same values as `java` does. Before the fix,
// `versioned` threw NullPointerException at `sb.append(i)` while `plain` and
// `firstLoopOnly` were correct.
//
// The failure needed three things at once, which is why it took a bisect:
//
//   * a VERSIONED artifact — `versioned` has a constant IV init and a runtime
//     limit, so `prove_trip_count_at_least` mints a `trip >= 4` witness. `plain`
//     differs only in taking its start value as a parameter, which makes
//     `constant_iv_init` answer "unknown"; the proof then refuses and the SAME
//     loop gets an unversioned unroll, which was always correct;
//   * entry through the OSR door — one long call, so the method tiers up while
//     the loop is on the stack. Driven by many short calls instead (the
//     invocation-count door) the same versioned artifact was correct;
//   * a second loop in the method, so the OSR request lands on a header whose
//     compiled state matters. `firstLoopOnly` is the same first loop with no
//     second one and was correct.
//
// The cause: `LoopXform::osr_entry_pc` answered the loop header's OSR entry
// with the pre-header GUARD, on the reasoning that re-evaluating it there is
// exactly what a fall-through entry does. That is true of the bytecode and
// false of the machine code — an OSR entry is only valid at a pc whose compiled
// state the entry trampoline can reconstruct from the interpreter frame, and
// the emitter publishes that state at loop headers, not at arbitrary
// straight-line pcs. The guard sits in the method's prologue, where `sb` can
// still live in a register the trampoline does not seed, so the loop ran with a
// null receiver. Every OSR entry now lands in the fallback copy.
public class LoopVersionOsrProbe {

    static long versioned(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        String s = sb.toString();
        Pattern p = Pattern.compile("(\\d+)");
        Matcher m = p.matcher(s);
        long sum = 0;
        while (m.find()) {
            sum += Long.parseLong(m.group(1));
        }
        return sum;
    }

    static long plain(int n, int start) {
        StringBuilder sb = new StringBuilder();
        for (int i = start; i <= n; i++) {
            sb.append(i).append(' ');
        }
        String s = sb.toString();
        Pattern p = Pattern.compile("(\\d+)");
        Matcher m = p.matcher(s);
        long sum = 0;
        while (m.find()) {
            sum += Long.parseLong(m.group(1));
        }
        return sum;
    }

    static long firstLoopOnly(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        return sb.length();
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        // One call each: the back-edge (OSR) door, which is the one that failed.
        System.out.println("versioned=" + versioned(n));
        System.out.println("plain=" + plain(n, 1));
        System.out.println("first=" + firstLoopOnly(n));
        // …and the invocation-count door over the same method, which never did.
        long hot = 0;
        for (int r = 0; r < 20000; r++) {
            hot += versioned(40);
        }
        System.out.println("hot=" + hot);
    }
}
