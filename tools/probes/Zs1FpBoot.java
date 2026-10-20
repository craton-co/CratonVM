// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s1/fp (2026-10-14): footprint rows for the zs round's footprint lane
// (docs/internal/zgc-round-zs-20261014/s1-fp-report.md). Max-RSS and CPU are
// read from outside (/usr/bin/time %M %U %S); this probe only does the work
// and prints deterministic stdout, so the harness can compare it byte for
// byte with HotSpot's.
//
// Usage:
//   Zs1FpBoot hello [holdMs]       "Hello, World!", then hold holdMs (default
//                                  0) so tools/rss-by-mapping.sh can snapshot
//                                  (marker `zs1fp-hold ready` on stderr).
//   Zs1FpBoot classes <n> <file>   Class.forName(name, false, null) for the
//                                  first n names of <file> (one per line; make
//                                  it ON HOTSPOT with `Zr7RsBoot list`), the
//                                  zr md row's load. n=0 is the noise control.
//   Zs1FpBoot alloc <rounds> <liveK>
//                                  allocation-heavy: each round builds and
//                                  drops a HashMap of liveK*1000 boxed keys to
//                                  String / int[] / small-object values, with
//                                  a deterministic LCG; prints a checksum.
//   Zs1FpBoot threads <k> <work>   k short-lived threads, one at a time, each
//                                  doing <work> rounds of VM-side work that
//                                  allocates in the VM's own allocator on that
//                                  thread (string building, a reflective
//                                  lookup, a thrown-and-caught exception with
//                                  its stack trace, an array copy): the
//                                  per-thread allocator-page row.
//
// stdout (identical on HotSpot and CratonVM):
//   Hello, World!                                   (hello)
//   zs1fp mode=hello done
//   zs1fp mode=classes n=<n> done
//   zs1fp mode=alloc rounds=<r> live_k=<k> check=<hex> done
//   zs1fp mode=threads k=<k> work=<w> check=<hex> done
// stderr: `zs1fp-status rss_kb= hwm_kb= anon_kb= file_kb= threads= loaded=`
// from /proc/self/status (-1 off Linux), after the work, and the classes
// mode's `zs1fp-classes listed= loaded=` line.
import java.io.BufferedReader;
import java.io.FileReader;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class Zs1FpBoot {
    static long field(String line) {
        String[] parts = line.trim().split("\\s+");
        return Long.parseLong(parts[1]);
    }

    static String status() {
        long rss = -1, hwm = -1, anon = -1, file = -1, threads = -1;
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.startsWith("VmRSS:")) {
                    rss = field(line);
                } else if (line.startsWith("VmHWM:")) {
                    hwm = field(line);
                } else if (line.startsWith("RssAnon:")) {
                    anon = field(line);
                } else if (line.startsWith("RssFile:")) {
                    file = field(line);
                } else if (line.startsWith("Threads:")) {
                    threads = field(line);
                }
            }
        } catch (Exception e) {
            // not Linux: keep -1
        }
        long loaded = -1;
        try {
            loaded = java.lang.management.ManagementFactory.getClassLoadingMXBean()
                    .getTotalLoadedClassCount();
        } catch (Throwable t) {
            // no management support: keep -1
        }
        return "rss_kb=" + rss + " hwm_kb=" + hwm + " anon_kb=" + anon + " file_kb=" + file
                + " threads=" + threads + " loaded=" + loaded;
    }

    static void classes(int n, String file) throws Exception {
        List<String> names = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(file))) {
            String line;
            while ((line = r.readLine()) != null) {
                line = line.trim();
                if (!line.isEmpty()) {
                    names.add(line);
                }
            }
        }
        int loaded = 0;
        for (int i = 0; i < n && i < names.size(); i++) {
            try {
                Class.forName(names.get(i), false, null);
                loaded++;
            } catch (Throwable t) {
                // a class this VM refuses to load: not counted
            }
        }
        System.err.println("zs1fp-classes listed=" + names.size() + " loaded=" + loaded);
    }

    static final class Node {
        final int a;
        final long b;
        Node next;

        Node(int a, long b) {
            this.a = a;
            this.b = b;
        }
    }

    static long alloc(int rounds, int liveK) {
        long seed = 0x5DEECE66DL;
        long check = 0;
        int live = liveK * 1000;
        for (int r = 0; r < rounds; r++) {
            Map<Integer, Object> m = new HashMap<>();
            for (int i = 0; i < live; i++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int k = (int) (seed >>> 33);
                Object v;
                switch ((int) ((seed >>> 13) & 3)) {
                    case 0:
                        v = "s" + (k & 0xFFFF);
                        break;
                    case 1:
                        v = new int[1 + (k & 63)];
                        break;
                    case 2:
                        Node n = new Node(k, seed);
                        n.next = new Node(k ^ 1, ~seed);
                        v = n;
                        break;
                    default:
                        v = new byte[16 + (k & 255)];
                        break;
                }
                m.put(k, v);
            }
            // Order-independent: a sum over every value's shape.
            long sum = m.size();
            for (Map.Entry<Integer, Object> e : m.entrySet()) {
                Object v = e.getValue();
                long h = e.getKey();
                if (v instanceof String) {
                    h += ((String) v).length();
                } else if (v instanceof int[]) {
                    h += ((int[]) v).length * 3L;
                } else if (v instanceof Node) {
                    h += ((Node) v).next.a;
                } else {
                    h += ((byte[]) v).length * 7L;
                }
                sum += h * 31 + (h >>> 7);
            }
            check = check * 1_000_003L + sum;
        }
        return check;
    }

    static long threads(int k, int work) throws Exception {
        long[] results = new long[k];
        for (int t = 0; t < k; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                long acc = id;
                for (int w = 0; w < work; w++) {
                    StringBuilder sb = new StringBuilder();
                    for (int i = 0; i < 16; i++) {
                        sb.append(id).append(':').append(w).append(',');
                    }
                    acc += sb.toString().hashCode();
                    try {
                        acc += String.class.getMethod("length").getName().length();
                    } catch (NoSuchMethodException e) {
                        acc -= 1;
                    }
                    try {
                        throw new IllegalStateException("w" + w);
                    } catch (IllegalStateException e) {
                        acc += e.getStackTrace().length > 0 ? 1 : 0;
                        acc += e.getMessage().length();
                    }
                    int[] a = new int[64 + (w & 63)];
                    int[] b = java.util.Arrays.copyOf(a, a.length * 2);
                    acc += b.length;
                }
                results[id] = acc;
            }, "zs1fp-" + t);
            th.start();
            th.join();
        }
        long check = 0;
        for (long r : results) {
            check = check * 31 + r;
        }
        return check;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "hello";
        switch (mode) {
            case "hello": {
                System.out.println("Hello, World!");
                long holdMs = args.length > 1 ? Long.parseLong(args[1]) : 0L;
                if (holdMs > 0) {
                    System.err.println("zs1fp-hold ready " + status());
                    Thread.sleep(holdMs);
                }
                System.err.println("zs1fp-status " + status());
                System.out.println("zs1fp mode=hello done");
                return;
            }
            case "classes": {
                int n = Integer.parseInt(args[1]);
                classes(n, args[2]);
                System.err.println("zs1fp-status " + status());
                System.out.println("zs1fp mode=classes n=" + n + " done");
                return;
            }
            case "alloc": {
                int rounds = Integer.parseInt(args[1]);
                int liveK = Integer.parseInt(args[2]);
                long check = alloc(rounds, liveK);
                System.err.println("zs1fp-status " + status());
                System.out.println("zs1fp mode=alloc rounds=" + rounds + " live_k=" + liveK
                        + " check=" + Long.toHexString(check) + " done");
                return;
            }
            case "threads": {
                int k = Integer.parseInt(args[1]);
                int work = Integer.parseInt(args[2]);
                long check = threads(k, work);
                System.err.println("zs1fp-status " + status());
                System.out.println("zs1fp mode=threads k=" + k + " work=" + work
                        + " check=" + Long.toHexString(check) + " done");
                return;
            }
            default:
                throw new IllegalArgumentException("mode: " + mode);
        }
    }
}
