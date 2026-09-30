import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntSupplier;

/**
 * gc-common w18-d: non-strong hidden classes (the `Lookup.defineHiddenClass`
 * default) unload when their mirror and instances die, not only with their
 * loader. This probe defines many hidden classes from one template, keeps
 * every 16th instance (and nothing else), collects, and then checks that
 * each kept instance still runs and still reads its own static state -- a
 * class unloaded under a live instance, or statics freed and then rooted,
 * shows up as a wrong value or a crash. It prints the same lines on HotSpot.
 */
public class HiddenClassChurnProbe {
    /** The template every hidden class is defined from. */
    public static class Template implements IntSupplier {
        static int counter;
        static final Object[] BOX = new Object[64];
        final int seed;

        public Template() {
            counter++;
            seed = counter * 7;
            BOX[seed & 63] = new byte[256];
        }

        @Override
        public int getAsInt() {
            return seed + counter + ((byte[]) BOX[seed & 63]).length;
        }
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 500;
        byte[] bytes;
        try (InputStream in = HiddenClassChurnProbe.class.getResourceAsStream(
                "HiddenClassChurnProbe$Template.class")) {
            bytes = in.readAllBytes();
        }
        long unloadedBefore = ManagementFactory.getClassLoadingMXBean().getUnloadedClassCount();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        List<IntSupplier> kept = new ArrayList<>();
        List<Integer> expected = new ArrayList<>();
        long defined = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < perRound; i++) {
                Class<?> c = lookup.defineHiddenClass(bytes, true).lookupClass();
                IntSupplier s = (IntSupplier) c.getConstructor().newInstance();
                defined++;
                if (i % 16 == 0) {
                    kept.add(s);
                    expected.add(s.getAsInt());
                }
            }
            System.gc();
            int bad = 0;
            for (int k = 0; k < kept.size(); k++) {
                if (kept.get(k).getAsInt() != expected.get(k)) {
                    bad++;
                }
            }
            System.out.println("round " + r + " defined=" + defined + " kept=" + kept.size()
                    + " bad=" + bad);
            if (bad != 0) {
                System.out.println("PROBE-FAIL");
                return;
            }
        }
        kept.clear();
        expected.clear();
        for (int i = 0; i < 3; i++) {
            System.gc();
        }
        long unloaded = ManagementFactory.getClassLoadingMXBean().getUnloadedClassCount() - unloadedBefore;
        // HotSpot unloads (nearly) all of them; the line only says whether any went.
        System.out.println("unloaded-any=" + (unloaded > 0));
        System.out.println("PROBE-OK");
    }
}
