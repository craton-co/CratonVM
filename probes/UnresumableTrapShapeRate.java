// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// What does `jit::ir_unresumable_protected_trap`'s refusal COST?
//
// The rule declines the optimizing tier to any method whose protected range
// carries BOTH a deopt-guarded opcode (array/field access, division) and a side
// effect, because the IR tier lowers those to a deopt guard it cannot resume.
// The refusal is correct. It had never been priced.
//
// A whole-JUnit-class wall-clock cannot answer this: the refusal moves 0.43% of
// admitted methods (62 of 14 421 across 30 netty classes), and a class's
// run-to-run spread is ~27%. This probe prices ONE method of the declined shape
// instead, run hot, so the number is the code-quality delta for the shape and
// not the scaffolding around it.
//
//   cratonvm --java-home <jdk> -cp <out> UnresumableTrapShapeRate 20000000 20
//   CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD=0 cratonvm ... (same)
//
// The switch is MEASUREMENT ONLY and unsound in general — see its doc. It is
// sound for THIS probe for a stated reason: the array index is masked into
// range and the reference is never null, so the guarded trap can never fire,
// which is the only thing the unsoundness needs.
//
// # Two arms are not enough — verify the probe is declined for the RIGHT reason
//
// The admission ladder is ORDERED, and several terms sit ahead of this rule.
// The one that bites a `try`/`finally` probe is RBC.6: if the handler reads a
// NON-PARAMETER local, the method is declined earlier and this rule never even
// runs. So `shaped()`'s `finally` touches only the parameter array. Confirm it
// with the verdict, never by assuming:
//
//   CRATONVM_DBG_IR_COMPILES=1 cratonvm ... 2>&1 | grep 'admission.*shaped'
//
// must say "an inline trap this tier deopts on ... sits in a protected range"
// with the guard ON, and "admitted to the optimizing pipeline" with it OFF.
// `CRATONVM_DBG_JIT_METHOD_STATS=1` must report `shape=1 refused=1` / `shape=1
// refused=0` for the two arms. A probe that is declined by a DIFFERENT term
// measures that term instead, and would report a confident wrong number.
//
// `flat()` is the control: same work, same loop, no `try` at all, so it is
// admitted in both arms and must not move between them. If it does, the host
// was loaded and the run is not comparable.
public final class UnresumableTrapShapeRate {

    static final int MASK = 1023;

    /// The declined shape, as a SMALL METHOD called in a loop rather than a
    /// loop inside a method. That is not cosmetic: a loop body tiers up through
    /// OSR, which is a different door and does not run the callee admission
    /// ladder this rule lives in — the first draft of this probe put the loop
    /// inside and reported `shape=0` because neither method was ever considered.
    /// It also matches the real population: the methods the rule declines in
    /// netty are small lifecycle bodies called repeatedly.
    ///
    /// The protected range covers `iaload` + `iastore` (deopt-guarded trap next
    /// to a side effect), and the handler reads only the parameter `a`, so
    /// RBC.6 does not decline it first.
    ///
    /// The index is masked and `a` is never null, so the guard can never trap —
    /// which is what makes the measurement arm sound HERE without making the
    /// switch sound in general.
    static int shapedOnce(int[] a, int k) {
        try {
            a[k] = a[k] + 1;
        } finally {
            a[MASK] = a[MASK] | 0;
        }
        return a[k];
    }

    static int shaped(int[] a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += shapedOnce(a, i & MASK);
        }
        return acc;
    }

    /// Control: the identical arithmetic with no exception table, so it is
    /// admitted to the optimizing tier in BOTH arms and prices the host.
    static int flatOnce(int[] a, int k) {
        a[k] = a[k] + 1;
        a[MASK] = a[MASK] | 0;
        return a[k];
    }

    static int flat(int[] a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += flatOnce(a, i & MASK);
        }
        return acc;
    }

    static long sink;

    interface Arm { int run(int[] a, int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        int[] a = new int[MASK + 1];
        for (int w = 0; w < reps; w++) { sink += arm.run(a, per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { sink += arm.run(a, per); }
        long t1 = System.nanoTime();
        System.out.printf("%-22s %9.3f ns/iter%n", name, (double) (t1 - t0) / (per * (long) reps));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        time("shaped (declined)", UnresumableTrapShapeRate::shaped, n, reps);
        time("flat (control)", UnresumableTrapShapeRate::flat, n, reps);
        System.out.println("sink=" + sink);
    }
}
