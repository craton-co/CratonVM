// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * WAS OPEN under --jdk-only (gone since the Reference Handler blocking-region fix; see
 * docs/internal/jdk-only/junit-discovery-fails-when-run-concurrently-under-jdk-only-FIXED-20260919.md): three threads each run `EngineTestKit` discovery on a
 * tiny JUnit class behind a barrier. HotSpot: badContainers=0/6 everywhere.
 * --jdk-only: about half of the iterations fail every thread at once with
 * "orderer may not add or remove test descriptors" (AbstractTestDescriptor.
 * orderChildren) -- correlated across threads, so a shared/global event rather
 * than a per-object race. netty's LeakPresenceExtensionTest reproduces it (1 of
 * 3). Not the HashSet iterator, LinkedHashSet/removeAll/containsAll, CHM
 * computeIfAbsent, identity hashes or a forced GC -- each probed alone, clean.
 * Needs the netty test classes on the classpath (apps/netty-suite-runner/
 * common.args).
 */
import org.junit.platform.testkit.engine.*;
import java.util.concurrent.*;
import java.util.*;
import static org.junit.platform.engine.discovery.DiscoverySelectors.selectClass;
public class JunitConcurrentDiscoveryProbe {
    public static void main(String[] a) throws Exception {
        String[] names = {"InheritedChildTest", "OuterNestedTest", "ExplicitExtensionTest"};
        int threads = 3, iters = 6;
        ExecutorService es = Executors.newFixedThreadPool(threads);
        List<Future<String>> fs = new ArrayList<>();
        CyclicBarrier bar = new CyclicBarrier(threads);
        for (int t = 0; t < threads; t++) {
            final String n = names[t];
            fs.add(es.submit(() -> {
                Class<?> c = Class.forName("io.netty.util.test.LeakPresenceExtensionTest$" + n);
                int bad = 0;
                for (int i = 0; i < iters; i++) {
                    bar.await();
                    EngineExecutionResults r = EngineTestKit.engine("junit-jupiter").selectors(selectClass(c)).execute();
                    long f = r.containerEvents().failed().count();
                    if (f != 0) bad++;
                }
                return n + " badContainers=" + bad + "/" + iters;
            }));
        }
        for (Future<String> f : fs) System.out.println(f.get());
        es.shutdown();
    }
}
