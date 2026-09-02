/**
 * Regression: a stack trace must not degrade when the methods on it tier up.
 *
 * Two defects from
 * `known-issues/jit-compiled-frame-has-no-line-and-no-inlined-callees-20260901.md`,
 * both of which only appear AFTER warm-up — that is, only in the runs anyone
 * cares about, and with nothing thrown or logged to say so:
 *
 *  1. A JIT-compiled frame carried `LINE_NUMBER_UNKNOWN`. The walk had the
 *     bytecode index all along (every live compiled frame publishes its
 *     safepoint id into `[rbp - sp_id_slot_off]`); it was simply not carried
 *     on the tuple the trace assembler was handed.
 *
 *  3. Once a method OSR-enters, its interpreter `Frame.pc` stops advancing, so
 *     every later trace reported the BACK-EDGE it tiered up at instead of the
 *     call site it is actually stopped on.
 *
 * Both assertions are written WITHOUT hardcoded line numbers, so editing this
 * file cannot make them vacuously true: the same site is reached twice, once
 * cold and once hot, and the two traces are required to agree. HotSpot passes
 * for the same reason CratonVM must — this is a fidelity vector, not a
 * CratonVM-specific one.
 *
 * The throw is EXPLICIT, not an implicit NPE, and that is load-bearing for the
 * oracle rather than a stylistic choice: HotSpot's `OmitStackTraceInFastThrow`
 * is on by default and replaces the trace of a REPEATED implicit exception with
 * a shared, frameless one. A first draft of this vector dereferenced null and
 * failed on HotSpot, not on CratonVM — the oracle has to be reachable without
 * a `-XX:` flag the suite does not pass.
 *
 * NOT asserted here, because they are still open on the page above: inlined
 * callees contribute no frame of their own (so `mid`/`outer` are absent from
 * the hot trace), and an exception raised BY compiled code is constructed
 * after the compiled frames have unwound (`probes/StackTraceCompiledCallee.java`).
 * A vector may only pin what actually holds; those two are tracked there.
 */
public class RJitStackTraceLines {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    static long sink;

    /** Absent frame, distinct from a present frame with an unknown line (-1). */
    static final int ABSENT = -999;

    static int lineOf(StackTraceElement[] st, String method) {
        for (StackTraceElement s : st) {
            if (s.getMethodName().equals(method)) return s.getLineNumber();
        }
        return ABSENT;
    }

    static int leaf(int i)  { if (i == 1) throw new IllegalStateException("boom"); return i; }
    static int mid(int i)   { return leaf(i) + 1; }
    static int outer(int i) { return mid(i) + 1; }

    /** The throw, wrapped so the trace always has the same shape. */
    static StackTraceElement[] probe() {
        try { outer(1); return null; }
        catch (IllegalStateException e) { return e.getStackTrace(); }
    }

    /**
     * ONE call site, reached from both the cold and the OSR-entered pass, so
     * the two traces are directly comparable with no constant written down.
     */
    static StackTraceElement[] hotDriver(int iters) {
        long a = 0;
        for (int r = 0; r < iters; r++) a += r ^ (r >>> 3);
        sink += a;
        return probe();
    }

    /** Warms leaf/mid/outer from a helper, so hotDriver stays interpreted. */
    static long warmCallees(int n) { long a = 0; for (int r = 0; r < n; r++) a += outer(r + 2); return a; }

    public static void main(String[] args) {
        StackTraceElement[] cold = hotDriver(1);
        check(cold != null, "cold: the probe did not throw");

        sink += warmCallees(400_000);
        StackTraceElement[] warmed = hotDriver(1);
        check(warmed != null, "warmed: the probe did not throw");

        // The loop count here is what makes hotDriver OSR-compile, so this
        // third trace is taken with hotDriver's interpreter pc parked at the
        // back-edge. Defect 3 is exactly this row.
        StackTraceElement[] osr = hotDriver(400_000);
        check(osr != null, "osr: the probe did not throw");

        int coldProbe = lineOf(cold, "probe");
        int coldDriver = lineOf(cold, "hotDriver");
        check(coldProbe > 0, "cold probe line: " + coldProbe);
        check(coldDriver > 0, "cold hotDriver line: " + coldDriver);

        // Defect 1: present, and with a real line, in every arm.
        for (String arm : new String[]{"warmed", "osr"}) {
            StackTraceElement[] st = arm.equals("warmed") ? warmed : osr;
            int p = lineOf(st, "probe");
            check(p != ABSENT, arm + ": probe frame is missing");
            check(p > 0, arm + ": probe has no line number (" + p + ")");
            check(p == coldProbe, arm + ": probe line " + p + " != cold " + coldProbe);
        }

        // Defect 3: the OSR-entered frame reports the call site it is stopped
        // on, not the back-edge it tiered up at. Same site, so same line.
        int osrDriver = lineOf(osr, "hotDriver");
        check(osrDriver != ABSENT, "osr: hotDriver frame is missing");
        check(osrDriver == coldDriver,
                "osr: hotDriver reports " + osrDriver + ", cold reports " + coldDriver
                        + " — a stale interpreter pc parked at the back-edge");

        // The deepest frame is the throw site itself, in every arm, on both VMs.
        for (String arm : new String[]{"cold", "warmed", "osr"}) {
            StackTraceElement[] st = arm.equals("cold") ? cold : arm.equals("warmed") ? warmed : osr;
            check(st.length > 0, arm + ": empty trace");
            check(st[0].getMethodName().equals("leaf"),
                    arm + ": innermost frame is " + st[0].getMethodName() + ", expected leaf");
            check(st[0].getLineNumber() > 0, arm + ": leaf has no line number");
        }

        System.out.println("CK RJitStackTraceLines sink=" + (sink != 0));
        System.out.println("PASS RJitStackTraceLines (" + checks + " checks)");
    }
}
