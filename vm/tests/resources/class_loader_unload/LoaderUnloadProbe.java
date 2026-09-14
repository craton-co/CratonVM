import java.io.ObjectStreamClass;
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.function.BooleanSupplier;

public final class LoaderUnloadProbe {
    private static final class BytesLoader extends ClassLoader {
        private final byte[] bytes;

        BytesLoader(byte[] bytes) {
            super(null);
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("unloadprobe.LoaderUnloadPayload")) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    /** Every `System.gc()` this probe has asked for, across all three phases. */
    private static int gcAttempts = 0;

    /**
     * Collect until `done` is true, or until `cap` attempts have been spent.
     *
     * WHY THIS IS A POLL AND NOT A COUNTED LOOP. The three phases below used to
     * run 30 + rounds*12 + 80 collections unconditionally -- 182 for the six
     * rounds the harness drives -- whatever the collector had already achieved.
     * That is affordable in a release build (~86 ms per `System.gc()` on this
     * fixture's heap, ~16 s for the whole probe) and it is not in a DEBUG build,
     * where one ZGC cycle costs ~2.5 s and the fixed schedule therefore needs
     * more than 400 s before it can even look at the weak references. The
     * harness runs `cargo test --workspace` in the debug profile, so the probe
     * could not finish inside its own 600 s cap there -- and nobody had seen
     * that, because `cargo test --workspace` is fail-fast and had been stopping
     * in `native-builtins` long before this target ran.
     *
     * Polling costs the gate nothing. The caps are the SAME numbers, so a VM
     * that needs every attempt still gets every attempt and a VM that never
     * reclaims still spends the full budget and still fails on the assertions
     * below. What changes is only that a VM which has already reclaimed stops
     * paying for collections whose outcome is known.
     */
    private static void gcUntil(BooleanSupplier done, int cap, int pressureBlocks)
            throws InterruptedException {
        for (int attempt = 0; attempt < cap; attempt++) {
            System.gc();
            gcAttempts++;
            if (pressureBlocks > 0) {
                byte[][] pressure = new byte[pressureBlocks][];
                for (int i = 0; i < pressure.length; i++) {
                    pressure[i] = new byte[128 * 1024];
                }
            }
            Thread.sleep(2);
            if (done.getAsBoolean()) {
                return;
            }
        }
    }

    private static long liveCount(List<? extends WeakReference<?>> refs) {
        return refs.stream().filter(ref -> ref.get() != null).count();
    }

    private static long exercise(
            byte[] bytes,
            int round,
            List<WeakReference<ClassLoader>> loaders,
            List<WeakReference<Class<?>>> classes) throws Exception {
        BytesLoader loader = new BytesLoader(bytes);
        Class<?> type = Class.forName("unloadprobe.LoaderUnloadPayload", true, loader);
        Object instance = type.getConstructor().newInstance();
        Method hot = type.getMethod("hot", int.class);
        Method staticHot = type.getMethod("staticHot", int.class);
        long checksum = 0;
        for (int i = 0; i < 700; i++) {
            checksum += (Integer) hot.invoke(instance, 20);
        }
        checksum += (Integer) staticHot.invoke(null, round);
        if (ObjectStreamClass.lookup(type) == null) {
            throw new AssertionError("missing serialization descriptor");
        }
        loaders.add(new WeakReference<>(loader));
        classes.add(new WeakReference<>(type));
        return checksum;
    }

    public static void main(String[] args) throws Exception {
        byte[] bytes = Files.readAllBytes(Path.of(args[0]));
        int rounds = Integer.parseInt(args.length > 1 ? args[1] : "12");
        ClassLoadingMXBean bean = ManagementFactory.getClassLoadingMXBean();
        List<WeakReference<ClassLoader>> loaders = new ArrayList<>();
        List<WeakReference<Class<?>>> classes = new ArrayList<>();

        // Warm up one-time management, reflection, and serialization classes
        // before measuring the bounded live-class count. The warm-up loader is
        // its own round, so "the warm-up has settled" is exactly "its loader and
        // its class are gone" -- the same question the gate asks later, which is
        // why this phase can poll on it rather than counting to thirty.
        long checksum = exercise(bytes, -1, loaders, classes);
        gcUntil(() -> liveCount(loaders) == 0 && liveCount(classes) == 0, 30, 0);
        loaders.clear();
        classes.clear();
        long unloadedBefore = bean.getUnloadedClassCount();
        int loadedBefore = bean.getLoadedClassCount();

        for (int round = 0; round < rounds; round++) {
            checksum += exercise(bytes, round, loaders, classes);
            // Everything from the PREVIOUS rounds must be gone before the next
            // one starts; the round just exercised is still strongly reachable
            // from this frame's locals, so it is deliberately not waited on.
            final int reclaimable = round;
            gcUntil(
                    () -> liveCount(loaders.subList(0, reclaimable)) == 0
                            && liveCount(classes.subList(0, reclaimable)) == 0,
                    12,
                    8);
        }

        // The gate's own question, asked directly: every loader and every class
        // gone, and the MXBean's unloaded count caught up with the rounds. The
        // count is part of the predicate on purpose -- a run that stopped as
        // soon as the weak references cleared could otherwise leave
        // `unloadedDelta` behind and fail an assertion it had already satisfied.
        gcUntil(
                () -> liveCount(loaders) == 0
                        && liveCount(classes) == 0
                        && bean.getUnloadedClassCount() - unloadedBefore >= rounds,
                80,
                16);

        long liveLoaders = liveCount(loaders);
        long liveClasses = liveCount(classes);
        long unloadedDelta = bean.getUnloadedClassCount() - unloadedBefore;
        int loadedAfter = bean.getLoadedClassCount();
        boolean ok = liveLoaders == 0
                && liveClasses == 0
                && unloadedDelta >= rounds
                && loadedAfter <= loadedBefore + 16;
        // `gcAttempts` is printed because it is the number this probe's cost is
        // proportional to, and because a rise in it is the early warning that
        // reclamation is getting harder -- the signal the old fixed schedule
        // could not produce.
        System.out.println("UNLOAD rounds=" + rounds
                + " liveLoaders=" + liveLoaders
                + " liveClasses=" + liveClasses
                + " unloadedDelta=" + unloadedDelta
                + " loadedBefore=" + loadedBefore
                + " loadedAfter=" + loadedAfter
                + " gcAttempts=" + gcAttempts
                + " checksum=" + checksum
                + " ok=" + ok);
        if (!ok) {
            throw new AssertionError("class-loader unloading failed");
        }
    }
}
