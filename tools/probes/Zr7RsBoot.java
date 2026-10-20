// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z7/rs (2026-10-13): boot footprint rows for
// docs/known-issues/footprint/boot-rss-by-owner-20261013.md.
//
// Usage:
//   Zr7RsBoot hello [holdMs]   print "Hello, World!", then hold, so
//                              tools/rss-by-mapping.sh can snapshot
//                              /proc/<pid>/smaps at a stable point (its marker
//                              `zr7rs-hold ready` goes to stderr just before
//                              the sleep; holdMs default 1500).
//   Zr7RsBoot gc [holdMs]      the same after one System.gc(): the first
//                              collection's side tables are resident too.
//   Zr7RsBoot list             print java.base's class names (sorted, one per
//                              line, from the jrt file system). Run it ON
//                              HOTSPOT into a file: on CratonVM a jrt mount
//                              reads the whole runtime image into the Java
//                              heap (see the page), which would swamp the
//                              `classes` row's RSS.
//   Zr7RsBoot classes <n> <file>
//                              the per-class resident cost: samples VmRSS,
//                              loads the first n names of <file> with
//                              Class.forName(name, false, null) (load and
//                              link, no <clinit>), samples again. n=0 is the
//                              noise control.
// stdout (identical on HotSpot and CratonVM):
//   Hello, World!            (hello / gc)
//   zr7rs mode=<m> done      (zr7rs mode=classes n=<n> done)
// stderr: the measurement lines, from /proc/self/status (-1 where it does not
// exist, i.e. not Linux):
//   zr7rs-hold ready rss_kb= hwm_kb= anon_kb= file_kb= shmem_kb= threads= used=
//   zr7rs-hold end   rss_kb= ... classes_loaded=<ClassLoadingMXBean total, read
//                    after the hold so its own classes are not in the snapshot>
//   zr7rs-classes n= listed= loaded= rss_before_kb= rss_after_kb= rss_delta_kb= per_class_b=
import java.io.BufferedReader;
import java.io.FileReader;

public class Zr7RsBoot {
    static String status() {
        long rss = -1, hwm = -1, anon = -1, file = -1, shmem = -1, threads = -1;
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
                } else if (line.startsWith("RssShmem:")) {
                    shmem = field(line);
                } else if (line.startsWith("Threads:")) {
                    threads = field(line);
                }
            }
        } catch (Exception e) {
            // not Linux: keep -1
        }
        Runtime rt = Runtime.getRuntime();
        return "rss_kb=" + rss + " hwm_kb=" + hwm + " anon_kb=" + anon + " file_kb=" + file
                + " shmem_kb=" + shmem + " threads=" + threads + " used="
                + (rt.totalMemory() - rt.freeMemory());
    }

    static long field(String line) {
        String[] parts = line.trim().split("\\s+");
        return Long.parseLong(parts[1]);
    }

    static long rssKb() {
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.startsWith("VmRSS:")) {
                    return field(line);
                }
            }
        } catch (Exception e) {
            // not Linux
        }
        return -1;
    }

    static void list() throws Exception {
        java.nio.file.FileSystem jrt =
                java.nio.file.FileSystems.getFileSystem(java.net.URI.create("jrt:/"));
        java.util.List<String> names = new java.util.ArrayList<>();
        java.nio.file.Path root = jrt.getPath("/modules/java.base");
        try (java.util.stream.Stream<java.nio.file.Path> s = java.nio.file.Files.walk(root)) {
            s.forEach(p -> {
                String f = root.relativize(p).toString();
                if (f.endsWith(".class") && !f.endsWith("module-info.class")) {
                    names.add(f.substring(0, f.length() - 6).replace('/', '.'));
                }
            });
        }
        java.util.Collections.sort(names);
        for (String n : names) {
            System.out.println(n);
        }
    }

    static void classes(int n, String file) throws Exception {
        java.util.List<String> names = new java.util.ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(file))) {
            String line;
            while ((line = r.readLine()) != null) {
                line = line.trim();
                if (!line.isEmpty()) {
                    names.add(line);
                }
            }
        }
        System.gc();
        long before = rssKb();
        int loaded = 0;
        for (int i = 0; i < n && i < names.size(); i++) {
            try {
                Class.forName(names.get(i), false, null);
                loaded++;
            } catch (Throwable t) {
                // a class this VM refuses to load: not counted
            }
        }
        long after = rssKb();
        long delta = (before < 0 || after < 0) ? -1 : after - before;
        System.err.println("zr7rs-classes n=" + n + " listed=" + names.size() + " loaded=" + loaded
                + " rss_before_kb=" + before + " rss_after_kb=" + after + " rss_delta_kb=" + delta
                + " per_class_b=" + (loaded > 0 && delta >= 0 ? delta * 1024 / loaded : -1));
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "hello";
        if (mode.equals("list")) {
            list();
            return;
        }
        if (mode.equals("classes")) {
            int n = Integer.parseInt(args[1]);
            classes(n, args[2]);
            System.out.println("zr7rs mode=classes n=" + n + " done");
            return;
        }
        long holdMs = args.length > 1 ? Long.parseLong(args[1]) : 1500L;
        if (mode.equals("gc")) {
            System.gc();
        } else if (!mode.equals("hello")) {
            throw new IllegalArgumentException("mode must be hello, gc, list or classes: " + mode);
        }
        // Before the marker, so a VM whose stdout is buffered has already
        // done the work of printing it when the snapshot is taken.
        System.out.println("Hello, World!");
        System.err.println("zr7rs-hold ready " + status());
        System.err.flush();
        Thread.sleep(holdMs);
        // After the snapshot on purpose: the management classes this loads
        // must not be in the measured resident set.
        long classes = -1;
        try {
            classes = java.lang.management.ManagementFactory.getClassLoadingMXBean()
                    .getTotalLoadedClassCount();
        } catch (Throwable t) {
            // no class-loading bean: keep -1
        }
        System.err.println("zr7rs-hold end " + status() + " classes_loaded=" + classes);
        System.out.println("zr7rs mode=" + mode + " done");
    }
}
