// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

/**
 * gci i46/lg ({@code gci-i44sp-xlog-safepoint-prints-nothing-20261016}
 * residuals 2 and 6): a single thread's {@code Thread.getStackTrace} is a
 * handshake on HotSpot 25 -- no safepoint, so neither a
 * {@code sun.rt.safepoints} count nor a {@code Safepoint "ThreadDump"} line --
 * while {@code System.gc()} is one safepoint each. Needs
 * {@code --add-exports java.management/sun.management=ALL-UNNAMED}; with
 * {@code -Xlog:safepoint:file=/tmp/lg46sp_%p.log -Dlg46.log=/tmp/lg46sp_%p.log}
 * the log is read too.
 *
 * <p>Args: {@code [n]} (default 20): {@code n} stack traces of a spinning
 * thread, then {@code n} {@code System.gc()} calls. stdout (predicates only):
 * <pre>
 * lg46 stack_traces_add_no_safepoint=true   (count moved by at most 1 over the traces)
 * lg46 gcs_counted=true                     (count moved by at least n over the collections)
 * lg46 no_threaddump_line=true              (no `Safepoint "ThreadDump"` line; `skipped` without the log)
 * </pre>
 * stderr (diagnostic): the raw counts.
 */
public class GciLg46SafepointCountProbe {
    private static volatile boolean stop;
    private static volatile long sink;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        Object rt = Class.forName("sun.management.ManagementFactoryHelper")
                .getMethod("getHotspotRuntimeMBean")
                .invoke(null);
        Method count = Class.forName("sun.management.HotspotRuntimeMBean").getMethod("getSafepointCount");

        Thread spin = new Thread(() -> {
            long x = 0;
            while (!stop) {
                x += System.nanoTime() & 7;
            }
            sink = x;
        }, "lg46-spin");
        spin.setDaemon(true);
        spin.start();
        Thread.sleep(100);

        long c0 = (Long) count.invoke(rt);
        int frames = 0;
        for (int i = 0; i < n; i++) {
            frames += spin.getStackTrace().length;
        }
        long c1 = (Long) count.invoke(rt);
        for (int i = 0; i < n; i++) {
            System.gc();
            Thread.sleep(2);
        }
        long c2 = (Long) count.invoke(rt);
        stop = true;
        spin.join();
        Thread.sleep(200);

        String template = System.getProperty("lg46.log");
        String lines = "skipped";
        if (template != null) {
            Path log = Path.of(template.replace("%p", Long.toString(ProcessHandle.current().pid())));
            List<String> all = Files.exists(log) ? Files.readAllLines(log) : List.of();
            long dumps = all.stream().filter(l -> l.contains("Safepoint \"ThreadDump\"")).count();
            System.err.println("lg46 ThreadDump lines=" + dumps + " of " + all.size());
            lines = Boolean.toString(dumps == 0);
        }
        System.err.println("lg46 raw count " + c0 + " -> " + c1 + " -> " + c2 + ", frames " + frames);
        System.out.println("lg46 stack_traces_add_no_safepoint=" + (c1 - c0 <= 1));
        System.out.println("lg46 gcs_counted=" + (c2 - c1 >= n));
        System.out.println("lg46 no_threaddump_line=" + lines);
    }
}
