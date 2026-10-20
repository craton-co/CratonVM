// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

/**
 * gci i46/lg ({@code gci-i45sl-xlog-file-output-appends-to-an-existing-log-20261016}):
 * an {@code -Xlog:gc:file=<name>} output against HotSpot 25's
 * {@code LogFileOutput} -- the existing file archived to {@code <name>.0} at
 * VM start ({@code filecount} default 5), truncated under {@code filecount=0},
 * rotated at {@code filesize}. Run with {@code -Dlg46.log=<name>} naming the
 * same file as the {@code -Xlog} option (a fixed name: no {@code %p}).
 *
 * <p>Args: {@code <mode> [n]}; each mode first runs {@code n} (default 3)
 * {@code System.gc()} calls (one {@code Pause Full (System.gc())} line each),
 * then reads the files and prints only predicates, so stdout is deterministic
 * on both VMs:
 * <pre>
 * seed   delete &lt;name&gt;.0 .. &lt;name&gt;.4 first   lg46 seeded=true
 * check  (after a seed row on the same name)      lg46 archive_exists=  lg46 archive_has_previous_run=
 *                                                 lg46 current_is_this_run=
 * trunc  (after a seed row; filecount=0)          lg46 no_archive=  lg46 current_is_this_run=
 * size   delete the archives first (filesize=)    lg46 rotated=  lg46 total_is_n=
 * none   no file read                             lg46 ran=true
 * </pre>
 * stderr (diagnostic): the line count of each file.
 */
public class GciLg46XlogFileRotationProbe {
    private static final String MARK = "Pause Full (System.gc())";
    // gci i47w/j46: `check` / `trunc` tell runs apart by the one `Using <collector>` line each run writes
    // first, and count this run's collections by their cause: CratonVM's G1 logs a `System.gc()` as
    // `Pause Young (...) (System.gc())` pauses (gci-i10dl item b, a separate open item), not one
    // `Pause Full`, which made `lg46_r_g1_check` DIFF although the file was archived and fresh.
    private static final String START = "][gc] Using ";
    private static final String CAUSE = "(System.gc())";

    private static Path archive(Path log, int i) {
        return Path.of(log.toString() + "." + i);
    }

    private static int count(Path p) throws Exception {
        return count(p, MARK);
    }

    private static int count(Path p, String needle) throws Exception {
        if (!Files.exists(p)) {
            return -1;
        }
        List<String> lines = Files.readAllLines(p);
        int c = 0;
        for (String l : lines) {
            if (l.contains(needle)) {
                c++;
            }
        }
        return c;
    }

    /** Exactly one run (one `Using` line) that logged at least {@code n} {@code System.gc()} pauses. */
    private static boolean oneRun(Path p, int n) throws Exception {
        return count(p, START) == 1 && count(p, CAUSE) >= n;
    }

    private static void deleteArchives(Path log) throws Exception {
        for (int i = 0; i < 5; i++) {
            Files.deleteIfExists(archive(log, i));
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "none";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        Path log = Path.of(System.getProperty("lg46.log", "/tmp/lg46.log"));
        if (mode.equals("seed") || mode.equals("size")) {
            deleteArchives(log);
        }
        for (int i = 0; i < n; i++) {
            System.gc();
            Thread.sleep(2);
        }
        // Let the last pause's lines land.
        Thread.sleep(200);
        int current = count(log);
        int a0 = count(archive(log, 0));
        System.err.println("lg46 " + mode + " current=" + current + " archive0=" + a0);
        switch (mode) {
            case "seed":
                System.out.println("lg46 seeded=true");
                break;
            case "check":
                System.out.println("lg46 archive_exists=" + (a0 >= 0));
                System.out.println("lg46 archive_has_previous_run=" + (count(archive(log, 0), START) >= 1));
                System.out.println("lg46 current_is_this_run=" + oneRun(log, n));
                break;
            case "trunc":
                System.out.println("lg46 no_archive=" + (a0 < 0));
                System.out.println("lg46 current_is_this_run=" + oneRun(log, n));
                break;
            case "size": {
                int total = Math.max(current, 0);
                for (int i = 0; i < 5; i++) {
                    int c = count(archive(log, i));
                    System.err.println("lg46 size archive" + i + "=" + c);
                    total += Math.max(c, 0);
                }
                System.out.println("lg46 rotated=" + (a0 >= 0));
                System.out.println("lg46 total_is_n=" + (total == n));
                break;
            }
            default:
                System.out.println("lg46 ran=true");
        }
    }
}
