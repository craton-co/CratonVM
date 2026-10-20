// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z3/fp (2026-10-12): resident-set rows for the ZGC footprint page
// (docs/known-issues/footprint/peak-rss-is-four-times-hotspot-20261012.md).
// Reads VmRSS / VmHWM from /proc/self/status at fixed points of three shapes,
// so the same source run on HotSpot (`java -XX:+UseZGC`) is the twin.
//
// Usage: Zr3FpRss <idle|burst|steady> [workUnits]
//   idle    boot, sleep 500 ms, sample.
//   burst   allocate min(3 x maxMemory, 2 GiB x units) of short-lived arrays
//           (64..4160 bytes) with a 1/16-of-max sliding window, sample, sleep
//           2 s, sample again: the after-idle row is what the uncommit policy
//           gives back (with ZUncommitDelay=300 s, neither VM should give
//           anything back in 2 s; -XX:ZUncommitDelay=1 is the positive control).
//   steady  build a live set of min(30 % of maxMemory, 256 MiB x units) in small
//           objects, then churn 2 x that through a sliding window while it is
//           held, sample: side tables sized by OBJECT COUNT (forwarding, dead
//           lists, bitmaps) show here.
//
// stdout: one deterministic line per shape (identical on both VMs, so the
// battery's SAME verdict is meaningful); stderr: the measurement lines,
//   zr3fp-rss shape=<s> point=<p> rss_kb=<n> hwm_kb=<n> total=<b> max=<b> used=<b>
// rss/hwm read -1 where /proc/self/status does not exist (not Linux).
import java.io.BufferedReader;
import java.io.FileReader;

public class Zr3FpRss {
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

    static void sample(String shape, String point) {
        long[] s = status();
        Runtime rt = Runtime.getRuntime();
        System.err.println("zr3fp-rss shape=" + shape + " point=" + point + " rss_kb=" + s[0]
                + " hwm_kb=" + s[1] + " total=" + rt.totalMemory() + " max=" + rt.maxMemory()
                + " used=" + (rt.totalMemory() - rt.freeMemory()));
    }

    static final class Node {
        long a;
        int b;
        Node next;

        Node(long a, int b, Node next) {
            this.a = a;
            this.b = b;
            this.next = next;
        }
    }

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "idle";
        int units = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        long max = Runtime.getRuntime().maxMemory();
        sample(shape, "boot");
        if (shape.equals("idle")) {
            Thread.sleep(500);
            sample(shape, "idle");
            System.out.println("zr3fp shape=idle ok");
            return;
        }
        if (shape.equals("burst")) {
            long total = Math.min(3 * max, 2L * 1024 * 1024 * 1024 * units);
            int window = (int) Math.max(64, Math.min(1 << 20, max / 16 / 2112));
            Object[] ring = new Object[window];
            long allocated = 0, sum = 0;
            int i = 0;
            while (allocated < total) {
                int len = 64 + (i % 64) * 64;
                byte[] b = new byte[len];
                b[len - 1] = (byte) i;
                sum += b[len - 1];
                ring[i % window] = b;
                allocated += len + 16;
                i++;
            }
            sink = ring;
            sample(shape, "after_burst");
            ring = null;
            sink = null;
            Thread.sleep(2000);
            sample(shape, "after_idle");
            System.out.println("zr3fp shape=burst ok sum=" + (sum & 0xffff));
            return;
        }
        if (shape.equals("steady")) {
            long live = Math.min(max * 3 / 10, 256L * 1024 * 1024 * units);
            // ~40 bytes a Node on either VM's layout, give or take; the count
            // is fixed from the budget so stdout does not depend on the VM.
            long nodes = live / 40;
            Node head = null;
            for (long n = 0; n < nodes; n++) {
                head = new Node(n, (int) n, head);
            }
            sample(shape, "live_built");
            int window = 1 << 14;
            Object[] ring = new Object[window];
            long churn = 2 * live, allocated = 0;
            int i = 0;
            while (allocated < churn) {
                ring[i & (window - 1)] = new Node(i, i, null);
                allocated += 40;
                i++;
            }
            sink = ring;
            long check = 0;
            int walked = 0;
            for (Node p = head; p != null && walked < 1000; p = p.next, walked++) {
                check += p.b;
            }
            sample(shape, "after_churn");
            System.out.println("zr3fp shape=steady ok nodes=" + nodes + " check=" + check);
            sink = head;
            return;
        }
        System.out.println("zr3fp unknown shape " + shape);
    }
}
