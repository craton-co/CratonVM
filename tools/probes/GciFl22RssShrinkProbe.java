// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedReader;
import java.io.FileReader;
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i22/fl: does the process give memory back after its old live set drops?
 *
 * <p>The shape of {@code tools/bench/GenR4W6OldShrinkProbe} (grow an old live set of
 * 256 KiB {@code byte[]}s with young garbage in between, tenure it with more garbage,
 * drop all but {@code keep} chunks in place, then allocate only young garbage), with
 * the resident set read from {@code /proc/self/status} ({@code VmRSS}, Linux; -1
 * elsewhere) and the committed heap from {@code MemoryMXBean}. Used by the
 * {@code old-give-back}, {@code g1-uncommit} and {@code gen-uncommit-keep-survivors}
 * blocks of {@code vi22-fl.list}: their gain is resident memory, which no stdout
 * line can show.
 *
 * <p>Usage: {@code GciFl22RssShrinkProbe <retainMiB> <keepChunks> <afterChurnMiB>}.
 *
 * <p>Stdout is deterministic (the checksum of the kept chunks and the bytes churned,
 * and whether every kept chunk is intact); the figures go to stderr as
 * {@code [fl22-rss] rss_start_kib=.. rss_peak_kib=.. rss_after_kib=.. committed_peak=..
 * committed_after=.. ms=..}.
 */
public class GciFl22RssShrinkProbe {
    static final int CHUNK = 256 * 1024;
    static final long MIB = 1024L * 1024L;
    static volatile Object sink;
    static long churned;

    static long rssKib() {
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.startsWith("VmRSS:")) {
                    return Long.parseLong(line.replaceAll("[^0-9]", ""));
                }
            }
        } catch (IOException | RuntimeException e) {
            return -1L;
        }
        return -1L;
    }

    static long committed() {
        return ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getCommitted();
    }

    static void churn(long bytes) {
        for (long b = 0; b < bytes; b += 65536) {
            byte[] a = new byte[65536];
            a[0] = (byte) (b >>> 16);
            sink = a;
            churned += 65536;
        }
    }

    public static void main(String[] args) {
        int retainMiB = args.length > 0 ? Integer.parseInt(args[0]) : 192;
        int keep = args.length > 1 ? Integer.parseInt(args[1]) : 16;
        int afterMiB = args.length > 2 ? Integer.parseInt(args[2]) : 3072;
        int retain = retainMiB * 4;
        if (keep > retain) {
            keep = retain;
        }
        long t0 = System.nanoTime();
        long rssStart = rssKib();
        List<byte[]> live = new ArrayList<>(retain);
        for (int i = 0; i < retain; i++) {
            byte[] a = new byte[CHUNK];
            a[0] = (byte) i;
            a[CHUNK - 1] = (byte) ~i;
            live.add(a);
            churn(MIB);
        }
        churn(1024L * MIB);
        long rssPeak = rssKib();
        long committedPeak = committed();

        live.subList(keep, retain).clear();

        long perRound = Math.max(1L, (long) afterMiB / 48L) * MIB;
        long rssAfter = rssPeak;
        long committedAfter = committedPeak;
        for (int round = 0; round < 48; round++) {
            churn(perRound);
            rssAfter = rssKib();
            committedAfter = committed();
        }

        long checksum = churned;
        boolean intact = true;
        for (int i = 0; i < live.size(); i++) {
            byte[] a = live.get(i);
            checksum += (a[0] & 0xFF) * 31L + (a[CHUNK - 1] & 0xFF);
            intact &= a[0] == (byte) i && a[CHUNK - 1] == (byte) ~i;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("rss-shrink retainMiB=" + retainMiB + " keep=" + keep + " afterMiB=" + afterMiB
                + " checksum=" + checksum + " intact=" + intact);
        System.err.println("[fl22-rss] rss_start_kib=" + rssStart + " rss_peak_kib=" + rssPeak
                + " rss_after_kib=" + rssAfter + " committed_peak=" + committedPeak
                + " committed_after=" + committedAfter + " ms=" + ms);
        System.out.println(intact ? "PROBE-OK" : "PROBE-FAIL");
    }
}
