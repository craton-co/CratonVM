package org.h2.test.store;

import java.lang.reflect.Method;
import org.h2.test.TestAll;

/**
 * Seed-pinning driver for TestRandomMapOps.
 *
 * TestRandomMapOps.testMap() walks 100 iterations whose seeds come from an
 * UNSEEDED java.util.Random, so a failure it reports ("seed:-4182... op:1213")
 * cannot be re-run by running the class again. testOps(fileName, size, seed)
 * is the deterministic unit underneath; this reaches it directly.
 *
 * argv: <seed> [size] [reps]
 */
public class SeededRandomMapOps {
    public static void main(String[] args) throws Exception {
        long seed = Long.parseLong(args[0]);
        int size = args.length > 1 ? Integer.parseInt(args[1]) : 3000;
        int reps = args.length > 2 ? Integer.parseInt(args[2]) : 1;
        TestRandomMapOps t = new TestRandomMapOps();
        TestAll cfg = new TestAll();
        cfg.big = true;
        t.init(cfg);
        t.config = cfg;
        Method m = TestRandomMapOps.class.getDeclaredMethod(
                "testOps", String.class, int.class, long.class);
        m.setAccessible(true);
        String fileName = "memFS:SeededRandomMapOps";
        for (int i = 0; i < reps; i++) {
            try {
                m.invoke(t, fileName, size, seed);
                System.out.println("SEEDED_PASS rep=" + i + " seed=" + seed);
            } catch (java.lang.reflect.InvocationTargetException e) {
                System.out.println("SEEDED_FAIL rep=" + i + " seed=" + seed
                        + " cause=" + e.getCause());
                e.getCause().printStackTrace(System.out);
                System.exit(1);
            } finally {
                org.h2.store.fs.FileUtils.delete(fileName);
            }
        }
        System.out.println("SEEDED_DONE");
    }
}
