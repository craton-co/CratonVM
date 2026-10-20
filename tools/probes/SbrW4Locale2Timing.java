// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.text.DateFormat;
import java.text.NumberFormat;
import java.util.Date;
import java.util.ListResourceBundle;
import java.util.Locale;
import java.util.ResourceBundle;
import java.util.TimeZone;

/**
 * SBR wave 4, lane LOCALE2: timing micro for the retired readers.
 *
 * Each kernel builds its formatter per call, so every iteration goes through
 * {@code LocaleResources} (number patterns, DecimalFormatSymbols data,
 * date-time patterns) or {@code ResourceBundle.getBundle}. Prints ms per round
 * for each kernel, {@code rounds} timed rounds after one warm-up round.
 *
 * Run: {@code cratonvm --jdk-only -cp . SbrW4Locale2Timing [iters] [rounds]}.
 */
public class SbrW4Locale2Timing {

    public static class Res extends ListResourceBundle {
        @Override
        protected Object[][] getContents() {
            return new Object[][] {{"greeting", "hello"}};
        }
    }

    static final Locale[] LS = {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN};
    static final Date WHEN = new Date(1700000000000L);
    static final TimeZone UTC = TimeZone.getTimeZone("UTC");

    static long nf(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h += NumberFormat.getInstance(LS[i & 3]).format(1234567.891 + i).length();
        }
        return h;
    }

    static long df(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            DateFormat d = DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT, LS[i & 3]);
            d.setTimeZone(UTC);
            h += d.format(WHEN).length();
        }
        return h;
    }

    static long sf(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h += String.format(LS[i & 3], "%,.2f %d", 1234567.891 + i, i).length();
        }
        return h;
    }

    static long rb(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h += ResourceBundle.getBundle("SbrW4Locale2Timing$Res", LS[i & 3]).getString("greeting").length();
        }
        return h;
    }

    interface K {
        long run(int n);
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        String[] names = {"nf", "df", "sf", "rb"};
        K[] ks = {SbrW4Locale2Timing::nf, SbrW4Locale2Timing::df, SbrW4Locale2Timing::sf, SbrW4Locale2Timing::rb};
        long sink = 0;
        for (int k = 0; k < ks.length; k++) {
            sink += ks[k].run(iters);
            StringBuilder sb = new StringBuilder(names[k]).append(" ms/round:");
            for (int r = 0; r < rounds; r++) {
                long t0 = System.nanoTime();
                sink += ks[k].run(iters);
                sb.append(' ').append((System.nanoTime() - t0) / 1_000_000);
            }
            System.out.println(sb);
        }
        System.out.println("sink " + sink);
    }
}
