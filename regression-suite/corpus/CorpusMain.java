import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Liveness wrapper for `main`-kind corpus workloads.
 *
 * WHY THIS EXISTS. A real application test class is not a regression-suite
 * vector: it has no "PASS &lt;Class&gt;" contract. `apps/h2database-suite-runner/
 * run-h2-suite.sh:174` classifies a workload purely by exit code, and on this
 * host that is demonstrably too weak a signal -- running
 * `org.h2.test.unit.TestBitStream` on HotSpot 25 exits 0 having printed
 * NOTHING AT ALL. An exit code of 0 with no output is indistinguishable from:
 *
 *   - the test ran and passed,
 *   - the class was found but its body was skipped,
 *   - a launcher that resolved no main class and exited 0 anyway.
 *
 * Those are different findings and must not collapse into one green cell.
 * This wrapper turns a silent workload into one that emits an explicit
 * start marker, an explicit end marker, and an explicit throw marker, so the
 * driver can tell "did not start" from "ran". The markers are printed by
 * BYTECODE the workload's own VM executes, so they are a statement about that
 * VM, not about the harness that launched it.
 *
 * Contract (all markers on stdout, one per line):
 *
 *   CORPUS-START &lt;fqcn&gt;               printed before the target's main is entered
 *   CORPUS-END &lt;fqcn&gt; completed=true  target's main returned normally
 *   CORPUS-END &lt;fqcn&gt; completed=exit  target called System.exit; seen via shutdown hook
 *   CORPUS-THROW &lt;fqcn&gt; &lt;Throwable&gt;   target's main threw; exit code 1
 *   CORPUS-NOMAIN &lt;fqcn&gt; &lt;reason&gt;     no usable `public static void main(String[])`
 *
 * DELIBERATELY NOT DONE HERE: no timing is printed. Wall-clock numbers taken
 * on this shared, loaded host are not a measurement (see the module doc,
 * docs/feature-designs/jdk-only-corpus-runner.md), and printing one invites
 * somebody to quote it.
 *
 * The normal-return marker is printed on the NORMAL path, not from the
 * shutdown hook, on purpose: if it came only from the hook then every corpus
 * run would silently depend on shutdown-hook delivery, and a VM that never
 * runs hooks would fail every workload for one shared reason. The hook is the
 * fallback for the System.exit path only, and it says so in its marker, so a
 * hook-delivery difference between two VMs shows up as its own distinct
 * signature rather than as 200 identical mystery failures.
 */
public final class CorpusMain {

    private static final AtomicBoolean DONE = new AtomicBoolean(false);

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("CORPUS-NOMAIN - no-target-class-argument");
            System.exit(3);
        }
        final String fqcn = args[0];
        final String[] rest = Arrays.copyOfRange(args, 1, args.length);

        Class<?> target;
        try {
            target = Class.forName(fqcn, true, CorpusMain.class.getClassLoader());
        } catch (Throwable t) {
            // Reported as NOMAIN, not THROW: a class that will not load has
            // not run, and "did not start" is a different verdict from "ran
            // and diverged". Collapsing them is the specific mistake this
            // wrapper exists to prevent.
            System.out.println("CORPUS-NOMAIN " + fqcn + " load-failed:" + t.getClass().getName()
                    + ":" + String.valueOf(t.getMessage()));
            System.out.flush();
            System.exit(4);
            return;
        }

        Method m;
        try {
            m = target.getMethod("main", String[].class);
        } catch (NoSuchMethodException e) {
            System.out.println("CORPUS-NOMAIN " + fqcn + " no-main-method");
            System.out.flush();
            System.exit(4);
            return;
        }
        if (!Modifier.isStatic(m.getModifiers())) {
            System.out.println("CORPUS-NOMAIN " + fqcn + " main-not-static");
            System.out.flush();
            System.exit(4);
            return;
        }

        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            if (DONE.compareAndSet(false, true)) {
                System.out.println("CORPUS-END " + fqcn + " completed=exit");
                System.out.flush();
            }
        }));

        System.out.println("CORPUS-START " + fqcn);
        System.out.flush();

        try {
            m.invoke(null, (Object) rest);
        } catch (InvocationTargetException ite) {
            Throwable cause = ite.getCause() == null ? ite : ite.getCause();
            DONE.set(true);
            System.out.println("CORPUS-THROW " + fqcn + " " + cause.getClass().getName());
            cause.printStackTrace(System.out);
            System.out.flush();
            System.exit(1);
            return;
        }

        if (DONE.compareAndSet(false, true)) {
            System.out.println("CORPUS-END " + fqcn + " completed=true");
        }
        System.out.flush();
    }

    private CorpusMain() {
    }
}
