/**
 * Alternate body for {@code RedefineCorrectnessProbe$Victim}, compiled into
 * `alt/` purely as a source of *different* class bytes.
 *
 * The nested class name must match the redefinition target EXACTLY
 * (`RedefineCorrectnessProbe$Victim`) — the VM rejects a redefinition whose
 * bytes name a different class, which is correct and is why this file mirrors
 * the outer class name rather than the cost probe's.
 *
 * `bump()` steps by 100 here instead of 1, so a stale body is visible in the
 * arithmetic rather than only in timing.
 */
public final class RedefineCorrectnessProbe {
    public static final class Victim {
        private int n;
        public int bump() {
            n = n + 100;
            return n;
        }
    }
}
