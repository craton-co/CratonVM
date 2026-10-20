/**
 * gcf f2/r (2026-10-01): the uncaught-exception report's JDK-frame module
 * prefix, {@code Suppressed:} blocks, long cause chains and cause cycles
 * against HotSpot (gcf-f1r-uncaught-report-lacks-module-prefix-and-suppressed).
 *
 * <p>No stdout: the report is on stderr. Compare the lines of stderr matching
 * {@code ^(Exception in thread|Caused by:|\t)} with the launcher's
 * {@code [cratonvm] main-vm run() returned Err: } prefix stripped, first block
 * only. Modes (first argument):
 * <ul>
 *   <li>{@code suppressed} (default): try-with-resources whose body dies in a
 *       {@code java.base} frame ({@code Thread.join(-1)}) and whose close
 *       throws: {@code at java.base/java.lang.Thread.join(..)}, a tab-indented
 *       {@code Suppressed:} block with {@code ... N more};</li>
 *   <li>{@code deep}: a cause chain twelve deep (all twelve printed);</li>
 *   <li>{@code cycle}: two causes naming each other
 *       ({@code [CIRCULAR REFERENCE: ..]}).</li>
 * </ul>
 */
public class GcfUncaughtSuppressedProbe {
    static void suppressed() throws Exception {
        try (AutoCloseable c = () -> {
            throw new IllegalStateException("close");
        }) {
            new Thread(() -> { }).join(-1);
        }
    }

    static void deep(int n) {
        if (n == 0) {
            throw new IllegalStateException("bottom");
        }
        try {
            deep(n - 1);
        } catch (RuntimeException e) {
            throw new RuntimeException("level " + n, e);
        }
    }

    static void cycle() {
        RuntimeException a = new RuntimeException("a");
        IllegalStateException b = new IllegalStateException("b");
        a.initCause(b);
        b.initCause(a);
        throw a;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "suppressed";
        switch (mode) {
            case "deep" -> deep(11);
            case "cycle" -> cycle();
            default -> suppressed();
        }
    }
}
