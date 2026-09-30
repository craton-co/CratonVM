// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 22, lane L6: timing probe for the not-entrant
// entry (jit/src/not_entrant.rs).
//
// Rows (ns per call, JIT on, medians of 5 slices):
//   direct   a compiled caller loop calling a non-spliced static method: the
//            cost of the five-byte entry NOP every method-entry body now
//            opens with. A/B the build before wave 22 against the build after
//            it: expected equal within noise (one NOP per call).
//   before   the same loop, running in a worker, before its callee's class is
//            retransformed (agent only).
//   after    the SAME running loop after the retransform: its baked call now
//            enters the patched body's stub and is re-dispatched through the
//            VM on every call (the caller is never re-bound; see the "re-bind
//            the caller" note on
//            docs/known-issues/interpreter/i21-L1-proposal-make-withdrawn-bodies-not-entrant-20260925.md).
//            Expected on CratonVM after wave 22: after >> before (the price of
//            correctness until a re-bind lands); before wave 22 after ~ before
//            but the loop ran the OLD callee (the `seen` line then says
//            old=...). HotSpot re-binds, so its after ~ before.
//            Wave 23 (lane L6): the stub FORWARDS once the new Callee.value is
//            compiled (NotEntrantRecord::forward_to): its first calls still
//            re-dispatch through the VM, every later one jumps from the stub
//            straight to the new body. Expected: after within ~1-3 ns of
//            before (a patched JMP, a load, two flag tests and an indirect
//            JMP per call), against wave 22's hundreds of ns. A/B the wave-22
//            and wave-23 builds on the `after` row; the `const`
//            NOT_ENTRANT_FORWARDING_ENABLED (jit/src/not_entrant.rs) = false
//            is the in-tree control. CRATONVM_DBG_DEOPT=1 shows the few
//            re-dispatches, the last one ending "later calls forward to the
//            current body".
//   seen     how many of the post-retransform calls returned the NEW value:
//            must be `new=all` on every VM (correctness, not timing). An
//            `old=N` on CratonVM after wave 22 meant the running loop SPLICED
//            the callee (shape 4, fixed in wave 23:
//            docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md);
//            the before/after rows then time the splice, not the stub.
//
// Wave 24 (lane L6): the before/after rows now come from ONE loop that runs
// across the retransform (see `running`). Wave 23's `after` slices ran in a
// second loop compiled after the retransform, which bound the new body
// directly, so that row never timed the stub at all. By design the forwarded
// call costs, over `before`: the patched entry's JMP rel32, then the stub's
// MOV R11 imm64, a load of the forward word, TEST/JZ, two (load address,
// CMP BYTE, JNE) pairs for the target's retired/superseded flags, and an
// indirect JMP -- about 11 instructions, all predictable, with L1-resident
// loads: expected `after - before` of 1-3 ns on this box. POSITIVE CONTROL
// that the row times the forward (JIT on, CRATONVM_DBG_JITC=1
// CRATONVM_DBG_DEOPT=1): one `[cratonvm-jitc] not-entrant pass: candidates=1
// patched=1 Published=0 ...` line (a SCOPED redefinition; a large candidates=
// count means some compile copied Callee's bytecode, the redefinition flushed
// the whole cache and the running loop left for the interpreter -- the after
// row then times a recompiled loop), a few `[cratonvm-deopt] not-entrant
// entry: re-dispatching NotEntrantCallBench$Callee.value(I)I` lines, the last
// ending `; later calls forward to the current body`, and NO `OSR-exit
// TRANSFER NotEntrantCallBench.running` line.
//
// Wave 25 (lane L6): when a CompiledMethod's `superseded` flag is the byte
// after its `retired` flag (the stub asks a live body), one CMP WORD tests
// both: the prefix is 8 instructions / 32 bytes instead of 11 / 41
// (not_entrant_stub_bytes, `flags_adjacent`). A/B the wave-24 and wave-25
// builds on the `after` row, JIT on, interleaved, medians of 5: expected
// `after` lower by a fraction of a ns (the removed load/compare/branch were
// L1-resident and predicted), `before`/`direct` unchanged, `seen new=all`.
// The rest of the gap to HotSpot is the patched JMP, the forward-word load
// and the indirect JMP, which only a re-bind of the caller removes (see the
// "Progress (wave 25)" section of
// docs/known-issues/interpreter/i21-L1-proposal-make-withdrawn-bodies-not-entrant-20260925.md).
//
// HotSpot 25 (JIT, i7-8550U, wave-24 shape, two runs): direct 0.16 / 0.14,
// before 1.89 / 1.84, after 2.65 / 2.62, seen new=all. (The wave-22 shape
// printed before 0.13, after 0.00: C2 folded the separate after loop.)
//
// SETUP for the before/after rows: an agent jar whose manifest has
//     Premain-Class: NotEntrantCallBench$Agent
//     Can-Retransform-Classes: true
// containing NotEntrantCallBench*.class, then
//     java|cratonvm [--compatible] -javaagent:bench.jar -cp bench.jar NotEntrantCallBench
// Without the agent only the direct row runs and the rest print "no agent".
// The transformer renames "QqA" to "QqB" in Callee's class bytes.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.Arrays;

public class NotEntrantCallBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        public static int value(int n) {
            int s = "QqA".charAt(2);
            for (int i = 0; i < n; i++) {
                s ^= i;
            }
            return s;
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"NotEntrantCallBench$Callee".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 2] = 'B';
                }
            }
            return out;
        }
    }

    static final int SLICE = 2_000_000;
    static volatile int sink;

    static long slice() {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < SLICE; i++) {
            acc += Callee.value(2);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        return c[c.length / 2] / (double) SLICE;
    }

    // One activation, ONE loop, that keeps running across the retransform
    // (wave 24): it times WARM discarded slices, 5 `before` slices, then spins
    // -- still inside the loop, so still in the same compiled body -- until
    // main has retransformed Callee, then times 5 `after` slices. The call to
    // Callee.value is the same compiled call site on both sides. (Wave 23's
    // version timed the `after` slices in a SECOND loop, which was compiled
    // after the retransform and so bound the new body directly: it never
    // measured the stub.)
    static final int WARM = 5;
    static final int NEW_VALUE = 'B' ^ 0 ^ 1;
    static volatile boolean warmedUp;
    static volatile boolean flipped;
    static long[] beforeRow = new long[5];
    static long[] afterRow = new long[5];
    static int newSeen;

    static void running() {
        long[] times = new long[WARM + 10];
        int done = 0;
        int i = 0;
        int fresh = 0;
        int acc = 0;
        long t0 = System.nanoTime();
        while (done < WARM + 10) {
            int v = Callee.value(2);
            acc += v;
            if (done >= WARM + 5 && v == NEW_VALUE) {
                fresh++;
            }
            if (++i == SLICE) {
                times[done++] = System.nanoTime() - t0;
                i = 0;
                if (done == WARM + 5) {
                    warmedUp = true;
                    while (!flipped) {
                        Thread.onSpinWait();
                    }
                }
                t0 = System.nanoTime();
            }
        }
        sink = acc;
        beforeRow = Arrays.copyOfRange(times, WARM, WARM + 5);
        afterRow = Arrays.copyOfRange(times, WARM + 5, WARM + 10);
        newSeen = fresh;
    }

    public static void main(String[] args) throws Exception {
        for (int w = 0; w < 5; w++) {
            slice();
        }
        long[] direct = new long[5];
        for (int s = 0; s < 5; s++) {
            direct[s] = slice();
        }
        System.out.printf(java.util.Locale.ROOT, "direct %.2f ns/call%n", median(direct));

        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("before no agent");
            System.out.println("after no agent");
            System.out.println("seen no agent");
            return;
        }
        Thread worker = new Thread(NotEntrantCallBench::running);
        worker.start();
        while (!warmedUp) {
            Thread.sleep(1);
        }
        i.addTransformer(new Rename(), true);
        i.retransformClasses(Callee.class);
        flipped = true;
        worker.join();
        System.out.printf(java.util.Locale.ROOT, "before %.2f ns/call%n", median(beforeRow));
        System.out.printf(java.util.Locale.ROOT, "after %.2f ns/call%n", median(afterRow));
        int total = 5 * SLICE;
        System.out.println("seen " + (newSeen == total ? "new=all" : "old=" + (total - newSeen)));
    }
}
