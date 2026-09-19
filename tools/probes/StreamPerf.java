// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Per-call cost of tiny stream pipelines, in the shape Mockito's Java9PlusLocationImpl runs per mock invocation
// (map / filter / skip / findFirst over a short list). Run under the default mode and --jdk-only.
// See docs/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Collectors;

public class StreamPerf {
    static long sink;
    interface Body { void run(int n); }

    static void time(String name, int n, Body b) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 4; r++) {
            long t = System.nanoTime();
            b.run(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        System.out.printf("%-46s %10.1f ns/call%n", name, best / (double) n);
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 50_000;
        List<String> list = new ArrayList<>();
        for (int i = 0; i < 12; i++) list.add("frame" + i);
        time("list.stream().findFirst()", n, k -> { for (int i = 0; i < k; i++) sink += list.stream().findFirst().get().length(); });
        time("stream.map.filter.skip(2).findFirst() [12]", n, k -> { for (int i = 0; i < k; i++) sink += list.stream().map(s -> s + "x").filter(s -> s.length() > 5).skip(2).findFirst().get().length(); });
        time("stream.filter.count() [12]", n, k -> { for (int i = 0; i < k; i++) sink += list.stream().filter(s -> s.endsWith("1")).count(); });
        time("stream.map.collect(toList()) [12]", n, k -> { for (int i = 0; i < k; i++) sink += list.stream().map(String::length).collect(Collectors.toList()).size(); });
        time("for-loop equivalent [12] (control)", n, k -> { for (int i = 0; i < k; i++) { int c = 0; for (String s : list) if (s.endsWith("1")) c++; sink += c; } });
        System.out.println("sink=" + sink);
    }
}
