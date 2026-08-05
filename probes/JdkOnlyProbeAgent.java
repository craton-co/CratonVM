import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * The `-javaagent:` half of {@link JdkOnlyPlatformProbe}'s `agent` section.
 *
 * <p>An agent is a strict-mode surface in its own right: the jar is opened
 * before the application class path exists, {@code premain} runs on the launch
 * thread before {@code main}, the agent's own classes are defined by the system
 * loader, and a registered {@link ClassFileTransformer} then sees every
 * subsequent definition. None of that is reachable from a probe that only calls
 * library methods.
 *
 * <p>It records what happened in statics rather than printing: the probe reads
 * them reflectively and folds them into its own transcript, so the agent
 * contributes no output of its own and cannot reorder the diff.
 *
 * <p>The transformer returns {@code null} for every class — it observes, it
 * does not rewrite. A transformer that edited bytes would make every downstream
 * section's result depend on whether the edit landed, which is a different
 * experiment.
 */
public final class JdkOnlyProbeAgent {
    private static final AtomicBoolean PREMAIN = new AtomicBoolean();
    private static final AtomicInteger TRANSFORMED = new AtomicInteger();
    private static final AtomicBoolean SELF_SEEN = new AtomicBoolean();
    private static volatile boolean canRetransform;

    private JdkOnlyProbeAgent() { }

    public static void premain(String args, Instrumentation inst) {
        PREMAIN.set(true);
        if (inst == null) return;
        canRetransform = inst.isRetransformClassesSupported();
        inst.addTransformer(new ClassFileTransformer() {
            @Override
            public byte[] transform(ClassLoader loader, String className, Class<?> classBeingRedefined,
                                    ProtectionDomain pd, byte[] classfileBuffer) {
                TRANSFORMED.incrementAndGet();
                // The probe's own main class is defined after premain returns,
                // so seeing it is the proof that the transformer is on the live
                // definition path rather than having been registered too late.
                if ("JdkOnlyPlatformProbe".equals(className)) SELF_SEEN.set(true);
                return null;
            }
        });
    }

    /** Also usable with the attach API, which routes to {@code agentmain}. */
    public static void agentmain(String args, Instrumentation inst) {
        premain(args, inst);
    }

    public static boolean premainRan() {
        return PREMAIN.get();
    }

    public static int transformedCount() {
        return TRANSFORMED.get();
    }

    public static boolean canRetransform() {
        return canRetransform;
    }

    public static boolean selfTransformSeen() {
        return SELF_SEEN.get();
    }
}
