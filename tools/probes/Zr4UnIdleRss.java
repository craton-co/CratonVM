// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z4/un (2026-10-12): does an IDLE ZGC heap give memory back to the OS?
// (docs/internal/gc/zgc-zr3fp-no-uncommitter-an-idle-heap-never-gives-back-FIXED-20261012.md)
//
// Usage: Zr4UnIdleRss [burstMiB] [idleSeconds]      (defaults 192, 6)
//   1. boot        sample
//   2. burst       retain burstMiB: 3/4 in 1 MiB byte[] (large-object end),
//                  1/4 in 4 KiB byte[] (small end); sample
//   3. dropped     drop everything, System.gc() twice; sample
//   4. idle        sleep idleSeconds with NO allocation; sample every second
//
// Run it with -XX:ZUncommitDelay=2 (an idle longer than the delay). HotSpot's
// ZUncommitter returns the dropped memory during step 4 (VmRSS falls, and
// Runtime.totalMemory() falls towards -Xms); a collector that gives back only
// inside a collection keeps step 3's RSS through the whole idle.
//
// stdout: one deterministic line (identical on both VMs).
// stderr: zr4un-rss point=<p> rss_kb=<n> hwm_kb=<n> total=<b> max=<b> used=<b>
// rss/hwm read -1 where /proc/self/status does not exist (not Linux).
import java.io.BufferedReader;
import java.io.FileReader;
import java.util.ArrayList;

public class Zr4UnIdleRss {
    static volatile Object sink;

    static long[] status() {
        long rss = -1, hwm = -1;
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.startsWith("VmRSS:")) {
                    rss = kb(line);
                } else if (line.startsWith("VmHWM:")) {
                    hwm = kb(line);
                }
            }
        } catch (Exception e) {
            // not Linux: keep -1
        }
        return new long[] {rss, hwm};
    }

    static long kb(String line) {
        String[] parts = line.trim().split("\\s+");
        return Long.parseLong(parts[1]);
    }

    static void sample(String point) {
        long[] s = status();
        Runtime rt = Runtime.getRuntime();
        System.err.println("zr4un-rss point=" + point + " rss_kb=" + s[0] + " hwm_kb=" + s[1]
                + " total=" + rt.totalMemory() + " max=" + rt.maxMemory()
                + " used=" + (rt.totalMemory() - rt.freeMemory()));
    }

    public static void main(String[] args) throws Exception {
        int burstMiB = args.length > 0 ? Integer.parseInt(args[0]) : 192;
        int idleSeconds = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        sample("boot");

        long checksum = 0;
        ArrayList<byte[]> hold = new ArrayList<>();
        int large = burstMiB * 3 / 4;
        for (int i = 0; i < large; i++) {
            byte[] b = new byte[1 << 20];
            b[i & 1023] = (byte) i;
            hold.add(b);
        }
        int smallCount = (burstMiB - large) * 256;
        for (int i = 0; i < smallCount; i++) {
            byte[] b = new byte[4096];
            b[i & 4095] = (byte) (i * 31);
            hold.add(b);
        }
        for (int i = 0; i < hold.size(); i++) {
            byte[] b = hold.get(i);
            checksum = checksum * 31 + b.length + b[i & (b.length - 1) & 1023];
        }
        int held = hold.size();
        sample("burst");

        hold = null;
        System.gc();
        System.gc();
        sample("dropped");

        for (int s = 1; s <= idleSeconds; s++) {
            Thread.sleep(1000);
            sample("idle" + s);
        }

        System.out.println("zr4un idle-rss burst_mib=" + burstMiB + " objects=" + held
                + " checksum=" + checksum);
    }
}
