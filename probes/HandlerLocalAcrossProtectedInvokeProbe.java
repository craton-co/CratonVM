/**
 * Regression probe for the exact shape that made
 * `SimpleApplicationEventMulticaster.invokeListener` NPE under CratonVM
 * (fixed-suite-bugs/springboot/springboot-rerun-20260728-small-residuals-cluster.md,
 * Case 3): a non-parameter local is assigned BEFORE a `try` region, the region
 * contains a virtual call that throws, and the handler reads that local back.
 *
 * That is the shape `local_handler_reads_unsafe_local` + `precise_exception_frames`
 * exist for, and the shape the `0xb6`/`0xb9` entries in
 * `precise_exception_frame_sites_supported` admit to compilation. If the
 * handler's view of the local is not restored, the local reads null/0 and the
 * handler NPEs — exactly the Spring symptom.
 *
 * Covers the local read back through invokevirtual, invokeinterface and
 * invokespecial throw sites, plus a nested try and a reassignment after the
 * protected call, and checks the local's VALUE (not merely non-null), so a
 * handler that sees the wrong-but-non-null object also fails.
 */
public final class HandlerLocalAcrossProtectedInvokeProbe {

    interface ErrorHandler { String id(); }

    static final class Handler implements ErrorHandler {
        private final String id;
        Handler(String id) { this.id = id; }
        @Override public String id() { return id; }
    }

    static class Listener {
        // Virtual, overridable, and always throws — an 0xb6 site inside the
        // protected range.
        void doInvoke(int i) { throw new IllegalStateException("boom " + i); }
    }

    static final class SubListener extends Listener {
        @Override void doInvoke(int i) { throw new IllegalStateException("sub " + i); }
    }

    interface Invoker { void doInvoke(int i); }

    static final class IfaceListener implements Invoker {
        @Override public void doInvoke(int i) { throw new IllegalStateException("iface " + i); }
    }

    private ErrorHandler errorHandler;

    ErrorHandler getErrorHandler() { return errorHandler; }

    /** Mirrors SimpleApplicationEventMulticaster.invokeListener exactly. */
    String invokeVirtual(Listener l, int i) {
        ErrorHandler h = getErrorHandler();
        if (h != null) {
            try {
                l.doInvoke(i);
            } catch (Throwable t) {
                return h.id();          // handler reads the pre-try local
            }
        } else {
            l.doInvoke(i);
        }
        return "no-throw";
    }

    String invokeInterface(Invoker l, int i) {
        ErrorHandler h = getErrorHandler();
        if (h != null) {
            try {
                l.doInvoke(i);          // 0xb9 inside the protected range
            } catch (Throwable t) {
                return h.id();
            }
        } else {
            l.doInvoke(i);
        }
        return "no-throw";
    }

    /** Local reassigned AFTER the protected call — the intra-block-def shape. */
    String reassignedAfterCall(Listener l, int i, ErrorHandler other) {
        ErrorHandler h = getErrorHandler();
        try {
            l.doInvoke(i);
            h = other;                  // never reached; must not kill the handler's view
        } catch (Throwable t) {
            return h.id();
        }
        return "no-throw";
    }

    /** Nested try; the inner handler reads a local defined outside both. */
    String nested(Listener l, int i) {
        ErrorHandler h = getErrorHandler();
        try {
            try {
                l.doInvoke(i);
            } catch (IllegalStateException inner) {
                return h.id();
            }
        } catch (Throwable outer) {
            return "outer";
        }
        return "no-throw";
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        HandlerLocalAcrossProtectedInvokeProbe p = new HandlerLocalAcrossProtectedInvokeProbe();
        Handler a = new Handler("A");
        Handler b = new Handler("B");
        Listener plain = new Listener();
        Listener sub = new SubListener();
        Invoker iface = new IfaceListener();
        long failures = 0;

        for (int i = 0; i < iterations; i++) {
            // Alternate the field so a memoized/constant-folded read is caught.
            p.errorHandler = (i & 1) == 0 ? a : b;
            String want = (i & 1) == 0 ? "A" : "B";

            if (!want.equals(p.invokeVirtual((i & 2) == 0 ? plain : sub, i))) failures++;
            if (!want.equals(p.invokeInterface(iface, i))) failures++;
            if (!want.equals(p.reassignedAfterCall(plain, i, a == p.errorHandler ? b : a))) failures++;
            if (!want.equals(p.nested(plain, i))) failures++;
        }

        // The null-errorHandler path must still propagate the original
        // exception. This is the actual Spring symptom: real Spring never sets
        // an `errorHandler`, so `ifnull` at offset 6 must skip the protected
        // region entirely — and the failure mode was that the compiled body
        // entered it anyway and then NPE'd on the same local inside the
        // handler. Run it many times, and AFTER the loop above, so the branch
        // has been profiled the other way and the method is long since hot.
        p.errorHandler = null;
        for (int i = 0; i < 10_000; i++) {
            try {
                p.invokeVirtual((i & 2) == 0 ? plain : sub, i);
                failures++;
                System.out.println("FAIL null-handler path swallowed the exception");
                break;
            } catch (IllegalStateException expected) {
                // correct
            } catch (Throwable t) {
                failures++;
                System.out.println("FAIL null-handler path threw " + t);
                break;
            }
            try {
                p.invokeInterface(iface, i);
                failures++;
                System.out.println("FAIL null-handler interface path swallowed the exception");
                break;
            } catch (IllegalStateException expected) {
                // correct
            } catch (Throwable t) {
                failures++;
                System.out.println("FAIL null-handler interface path threw " + t);
                break;
            }
        }

        if (failures != 0) {
            throw new AssertionError("handler-local failures: " + failures);
        }
        System.out.println("HANDLER_LOCAL_ACROSS_PROTECTED_INVOKE_OK iterations=" + iterations);
    }
}
