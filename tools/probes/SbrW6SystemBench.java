// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.EnumSet;
import java.util.List;
import java.util.StringJoiner;
import java.util.concurrent.Executors;
import java.util.function.IntFunction;

/**
 * SBR wave 6, lane {@code system}: the cost of the retired rows, in ns/op,
 * three rounds per row. Rows sit on the hot path of the JDK itself
 * ({@code System.getProperty}, {@code lineSeparator}, {@code UnixPath}'s
 * byte/string conversions, {@code StringJoiner}, enum constant lookup,
 * thread start through a container). Run the same way on the control and the
 * lane binary, interleaved, and read RANGES.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6SystemBench [rounds [row-name-substring]]
 * </pre>
 */
public class SbrW6SystemBench {
    enum Color { RED, GREEN, BLUE }

    static volatile Object sink;

    static String filter = "";

    static void bench(String name, int iters, int rounds, Runnable r) {
        if (!name.contains(filter)) return;
        for (int i = 0; i < Math.max(iters / 4, 1); i++) r.run();
        StringBuilder sb = new StringBuilder();
        for (int round = 0; round < rounds; round++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) r.run();
            long dt = System.nanoTime() - t0;
            sb.append(String.format(" %.0f", (double) dt / iters));
        }
        System.out.println(String.format("%-34s ns/op:%s", name, sb));
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        if (args.length > 1) filter = args[1];
        Path base = Paths.get("/tmp");
        bench("getProperty hit", 400_000, rounds, () -> sink = System.getProperty("os.name"));
        bench("getProperty miss", 400_000, rounds, () -> sink = System.getProperty("sbrw6.nope"));
        bench("getProperty(k,def)", 400_000, rounds, () -> sink = System.getProperty("sbrw6.nope", "d"));
        bench("lineSeparator", 2_000_000, rounds, () -> sink = System.lineSeparator());
        bench("getenv(name)", 200_000, rounds, () -> sink = System.getenv("PATH"));
        bench("getenv().get(name)", 200_000, rounds, () -> sink = System.getenv().get("PATH"));
        bench("Paths.get+toString", 200_000, rounds, () -> sink = Paths.get("/tmp/a/b/c.txt").toString());
        bench("path resolve+toString", 200_000, rounds, () -> sink = base.resolve("x/y").toString());
        bench("Files.exists", 40_000, rounds, () -> sink = Files.exists(base));
        bench("String.join 3", 400_000, rounds, () -> sink = String.join("-", "a", "b", "c"));
        bench("StringJoiner 3", 400_000, rounds, () -> {
            StringJoiner j = new StringJoiner(",", "[", "]");
            j.add("a").add("b").add("c");
            sink = j.toString();
        });
        bench("EnumSet.allOf", 400_000, rounds, () -> sink = EnumSet.allOf(Color.class));
        bench("Enum.valueOf", 400_000, rounds, () -> sink = Enum.valueOf(Color.class, "GREEN"));
        bench("System.getLogger cached", 200_000, rounds, () -> sink = System.getLogger("sbrw6.bench"));
        bench("virtual thread start+join", 4_000, rounds, () -> {
            try {
                Thread t = Thread.ofVirtual().unstarted(() -> { });
                t.start();
                t.join();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        bench("thread-per-task submit+get", 2_000, rounds, () -> {
            try (var ex = Executors.newThreadPerTaskExecutor(Thread.ofPlatform().factory())) {
                ex.submit(() -> 1).get();
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        bench("System.gc", 20, rounds, () -> System.gc());
    }
}
