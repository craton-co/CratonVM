// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.time.Duration;
import java.time.temporal.ChronoUnit;

/**
 * A merge whose phi copies alias in REGISTERS must still be a parallel copy.
 *
 * The optimizing tier sequentialises phi copies over FRAME WORDS
 * (`resolve_parallel_copy`), then reads a source from its assigned GPR and
 * publishes a destination into the phi's assigned GPR. Two DISTINCT frame
 * words can be homed in the SAME register -- the allocator sees a phi's live
 * range as starting after the merge and a dying source's as ending at it, so
 * they do not interfere by its reckoning -- and then an earlier copy's publish
 * overwrites a register a later copy still has to read.
 *
 * `shape()` below is `java.time.Duration.toNanos()` bytecode-for-bytecode: two
 * long locals, one seeded from a `long` field and one from an `int` field via
 * `i2l`, both reassigned inside a conditional block, both read after the merge.
 * Measured on a release binary, 2026-09-05, before the fix:
 *
 *   PhiCopyRegisterAliasProbe.shape : wrong from call 3,000 of 40,000 --
 *       returned `seconds * 1e9 + seconds` instead of `seconds * 1e9 + nanos`,
 *       because the else-arm emitted `mov rbx,rax` (publish phi_seconds, whose
 *       GPR is RBX) and then `mov rax,rbx` (read phi_nanos's SOURCE, whose GPR
 *       is also RBX).
 *   Duration.toNanos()              : 198,356 wrong answers in 200,000 calls,
 *       which made `ChronoUnit.MILLIS.getDuration().toNanos()` zero and
 *       `LocalDateTime.truncatedTo(MILLIS)` throw `ArithmeticException:
 *       / by zero` out of `LocalTime.truncatedTo` -- two failures in
 *       hibernate-reactive's `BasicTypesAndCallbacksForAllDBsTest`.
 *
 * `CRATONVM_JIT_IR_PHI_COPY_REGS=0` and `CRATONVM_JIT_IR_PHI_RESIDENCY=0` each
 * made both rows read zero, which is what named the register layer as owner.
 * HotSpot answers zero for every row.
 *
 * The three control shapes must stay here: each removes exactly one of the
 * three ingredients, and each was clean BEFORE the fix. A regression that
 * brought the bug back while leaving them clean would otherwise look like a
 * general phi failure rather than this specific aliasing one.
 */
public class PhiCopyRegisterAliasProbe {

    private final long seconds;
    private final int nanos;
    private final long nanosAsLong;

    PhiCopyRegisterAliasProbe(long seconds, int nanos) {
        this.seconds = seconds;
        this.nanos = nanos;
        this.nanosAsLong = nanos;
    }

    /** The failing shape: int-field source, both locals reassigned, merge. */
    long shape() {
        long tempSeconds = seconds;
        long tempNanos = nanos;
        if (tempSeconds < 0) {
            tempSeconds = tempSeconds + 1;
            tempNanos = tempNanos - 1000000000L;
        }
        return tempSeconds * 1000000000L + tempNanos;
    }

    /** Control: the second local comes from a `long` field, so no `i2l`. */
    long controlLongField() {
        long tempSeconds = seconds;
        long tempNanos = nanosAsLong;
        if (tempSeconds < 0) {
            tempSeconds = tempSeconds + 1;
            tempNanos = tempNanos - 1000000000L;
        }
        return tempSeconds * 1000000000L + tempNanos;
    }

    /** Control: only one local is reassigned, so there is one phi, not two. */
    long controlOnePhi() {
        long tempSeconds = seconds;
        long tempNanos = nanos;
        if (tempSeconds < 0) {
            tempNanos = tempNanos - 1000000000L;
        }
        return tempSeconds * 1000000000L + tempNanos;
    }

    /** Control: no conditional block at all, so there is no merge. */
    long controlNoBranch() {
        long tempSeconds = seconds;
        long tempNanos = nanos;
        return tempSeconds * 1000000000L + tempNanos;
    }

    private static int census(String label, long want, java.util.function.LongSupplier f, int n) {
        int wrong = 0;
        int firstAt = -1;
        for (int i = 0; i < n; i++) {
            long got = f.getAsLong();
            if (got != want) {
                if (firstAt < 0) {
                    firstAt = i;
                }
                wrong++;
            }
        }
        System.out.println(label + " wrong=" + wrong + "/" + n + " first=" + firstAt);
        return wrong;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 40000;
        PhiCopyRegisterAliasProbe o = new PhiCopyRegisterAliasProbe(2L, 5);
        long want = 2000000005L;

        int wrong = 0;
        wrong += census("shape", want, o::shape, n);
        wrong += census("controlLongField", want, o::controlLongField, n);
        wrong += census("controlOnePhi", want, o::controlOnePhi, n);
        wrong += census("controlNoBranch", want, o::controlNoBranch, n);

        // The JDK method the shape was taken from, through its real caller.
        wrong += census("Duration.toNanos", 1000000L,
                () -> ChronoUnit.MILLIS.getDuration().toNanos(), n);
        wrong += census("Duration.ofMillis(1).toNanos", 1000000L,
                () -> Duration.ofMillis(1).toNanos(), n);

        // `LocalTime.truncatedTo` divides by `unit.getDuration().toNanos()`, so
        // a zero there is an ArithmeticException, not a wrong number.
        int threw = 0;
        for (int i = 0; i < n / 10; i++) {
            try {
                java.time.LocalDateTime.now().truncatedTo(ChronoUnit.MILLIS);
            } catch (ArithmeticException e) {
                threw++;
            }
        }
        System.out.println("truncatedTo threw=" + threw + "/" + (n / 10));
        wrong += threw;

        if (wrong == 0) {
            System.out.println("PHI_COPY_REGISTER_ALIAS_OK");
        } else {
            System.out.println("PHI_COPY_REGISTER_ALIAS_DIVERGED wrong=" + wrong);
        }
    }
}
