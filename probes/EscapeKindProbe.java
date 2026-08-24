/**
 * WHEN, and through WHICH call kind, does an implicit trap raised inside a
 * compiled callee stop being caught by the CALLER's own `try`?
 *
 * `ExcTableDirectCallOracle` pins the callee's OWN handler and finds nothing
 * wrong with it. This pins the other half, which no probe covered: an
 * exception that is NOT caught in the callee and has to leave a baked direct
 * `CALL` and land on the caller's handler. The ban that
 * `direct_call_exc_table_publish_enabled` lifted named exactly this ("a raw
 * `CALL` has no Rust frame to notice the `i64::MIN` sentinel"), and the
 * self-catch cases pass either way, so a probe that only tests those reports
 * a clean bill on a broken tree.
 *
 * An outer `try` in the driver counts every escape and records the first index,
 * so "the handler never worked" and "the handler stopped working when the
 * caller was compiled" are distinguishable. `first` landing on a tiering
 * threshold is the tell.
 *
 * One arm per call kind, because the gates differ:
 *   static     invokestatic  -> `direct_call_exc_table_publish_enabled`
 *   special    invokespecial -> the same gate
 *   virtual    invokevirtual -> `mic_publish_exception_table_callees`
 *   iface      invokeinterface -> the same MIC gate
 *   notable    invokestatic to a callee with NO exception table (the control:
 *              this one was bindable before either gate was lifted)
 *
 *   java -cp out EscapeKindProbe [arm] [n]
 */
public final class EscapeKindProbe {

    static long sink;

    interface Op { int apply(int i); }

    static final class Impl implements Op {
        @Override public int apply(int i) {
            try { return 10 / (i - i); }
            catch (IllegalStateException e) { return -1; }
        }
        int applyVirtual(int i) {
            try { return 10 / (i - i); }
            catch (IllegalStateException e) { return -1; }
        }
        private int applySpecial(int i) {
            try { return 10 / (i - i); }
            catch (IllegalStateException e) { return -1; }
        }
        int callSpecial(int i) { return applySpecial(i); }
    }

    static final Impl IMPL = new Impl();
    static final Op OP = IMPL;

    /** Declares a handler of the WRONG type, so the trap propagates out. */
    static int guardedWrongType(int i) {
        try { return 10 / (i - i); }
        catch (IllegalStateException e) { return -1; }
    }

    /** No exception table at all. */
    static int bare(int i) { return 10 / (i - i); }

    static long callerStatic(int i)  { try { return guardedWrongType(i); } catch (ArithmeticException e) { return 7; } }
    static long callerNotable(int i) { try { return bare(i); }            catch (ArithmeticException e) { return 7; } }
    static long callerVirtual(int i) { try { return IMPL.applyVirtual(i);} catch (ArithmeticException e) { return 7; } }
    static long callerIface(int i)   { try { return OP.apply(i); }        catch (ArithmeticException e) { return 7; } }
    static long callerSpecial(int i) { try { return IMPL.callSpecial(i);} catch (ArithmeticException e) { return 7; } }

    public static void main(String[] args) {
        String arm = args.length > 0 ? args[0] : "static";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        long got = 0;
        int escapes = 0, first = -1, last = -1;
        for (int i = 0; i < n; i++) {
            try {
                switch (arm) {
                    case "static":  got += callerStatic(i); break;
                    case "notable": got += callerNotable(i); break;
                    case "virtual": got += callerVirtual(i); break;
                    case "iface":   got += callerIface(i); break;
                    case "special": got += callerSpecial(i); break;
                    default: throw new IllegalArgumentException(arm);
                }
            } catch (ArithmeticException e) {
                escapes++;
                if (first < 0) { first = i; }
                last = i;
                got += 7;
            }
        }
        sink = got;
        System.out.println(arm + ": n=" + n + " escapes=" + escapes + " first=" + first + " last=" + last
                + " total=" + got + " (want " + (7L * n) + ") " + (got == 7L * n ? "SUM-OK" : "SUM-WRONG"));
    }
}
