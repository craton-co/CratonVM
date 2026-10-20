// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4, wave 90, lane L5 -- the cost of DEFINING a JDK
// (bootstrap, java.base) class, with and without linking it. Each class is
// loaded once, cold, so a row is a distribution over different classes, not
// repetitions of one: compare the same row across runs, not across rows.
//
//   boot-load       Class.forName(name, false, null): load only. HotSpot does
//                   not verify a bootstrap class at all; CratonVM ran the full
//                   Pass 3 walk (its type maps) at define, under the class-
//                   manager write lock. CRATONVM_DEFINE_BOOT_MAPS_AT_LINK=1
//                   leaves the walk to the class's link, so this row should
//                   drop with it.
//   boot-load-init  Class.forName(name, true, null) on a second list: load,
//                   link and initialize. With the switch the walk moves into
//                   the link, so this row should stay roughly level.
//
// Output (STDOUT):
//   "row <step> min <ns> med <ns> ns/op"  over the classes of the row
//   "total <step> <ns> ns"                the row's sum (the number to compare)
//   "checksum <step> <n>"                 classes found (deterministic)
//
// Run:
//   $J/javac -d out L5W90BootDefineBench.java
//   $J/java -cp out L5W90BootDefineBench                                     # HotSpot reference
//   $B --java-home $J/.. -cp out L5W90BootDefineBench
//   CRATONVM_DEFINE_BOOT_MAPS_AT_LINK=1 $B --java-home $J/.. -cp out L5W90BootDefineBench
//   (each also with --compatible; run each arm several times, interleaved:
//   a single cold pass is noisy). Phase split (the `other-define` row holds
//   these defines, `verify-*` rows count only defineClass natives):
//   CRATONVM_DBG_DEFINE_CENSUS=1 $B --java-home $J/.. -cp out L5W90BootDefineBench 2>census.txt
//
// HOTSPOT_EXPECTED_BEGIN
// checksum boot-load 48
// checksum boot-load-init 24
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.Locale;

public class L5W90BootDefineBench {
    // java.base classes that a hello-world start does not load (mostly), each
    // present in every JDK since 8 at least.
    static final String[] LOAD = {
        "java.util.concurrent.ConcurrentSkipListMap",
        "java.util.concurrent.ConcurrentSkipListSet",
        "java.util.concurrent.ConcurrentLinkedDeque",
        "java.util.concurrent.LinkedTransferQueue",
        "java.util.concurrent.PriorityBlockingQueue",
        "java.util.concurrent.DelayQueue",
        "java.util.concurrent.Exchanger",
        "java.util.concurrent.Phaser",
        "java.util.concurrent.CyclicBarrier",
        "java.util.concurrent.Semaphore",
        "java.util.concurrent.atomic.LongAdder",
        "java.util.concurrent.atomic.DoubleAdder",
        "java.util.concurrent.atomic.LongAccumulator",
        "java.util.concurrent.atomic.DoubleAccumulator",
        "java.util.concurrent.atomic.AtomicLongArray",
        "java.util.concurrent.locks.StampedLock",
        "java.time.format.DateTimeFormatterBuilder",
        "java.time.temporal.IsoFields",
        "java.time.temporal.WeekFields",
        "java.time.chrono.JapaneseChronology",
        "java.time.chrono.HijrahChronology",
        "java.time.chrono.MinguoChronology",
        "java.time.chrono.ThaiBuddhistChronology",
        "java.time.Period",
        "java.time.Year",
        "java.time.YearMonth",
        "java.time.MonthDay",
        "java.time.OffsetTime",
        "java.time.OffsetDateTime",
        "java.text.DecimalFormat",
        "java.text.SimpleDateFormat",
        "java.text.MessageFormat",
        "java.text.ChoiceFormat",
        "java.text.Collator",
        "java.text.RuleBasedCollator",
        "java.text.BreakIterator",
        "java.text.Normalizer",
        "java.text.Bidi",
        "java.util.zip.Deflater",
        "java.util.zip.Adler32",
        "java.util.zip.GZIPOutputStream",
        "java.util.BitSet",
        "java.util.Scanner",
        "java.util.Currency",
        "java.util.Timer",
        "java.util.PropertyResourceBundle",
        "java.util.Stack",
        "java.util.Base64",
    };

    // A second list for the load + link + initialize row (trivial or no
    // static initializers).
    static final String[] INIT = {
        "java.util.zip.ZipOutputStream",
        "java.util.zip.CheckedInputStream",
        "java.util.zip.DeflaterOutputStream",
        "java.io.PushbackReader",
        "java.io.LineNumberReader",
        "java.io.StreamTokenizer",
        "java.io.SequenceInputStream",
        "java.io.PipedInputStream",
        "java.io.PipedOutputStream",
        "java.io.CharArrayWriter",
        "java.io.CharArrayReader",
        "java.io.PushbackInputStream",
        "java.net.URLDecoder",
        "java.net.HttpCookie",
        "java.math.MathContext",
        "java.util.EventObject",
        "java.util.Observable",
        "java.util.Dictionary",
        "java.util.AbstractQueue",
        "java.util.zip.CRC32",
        "java.util.zip.CheckedOutputStream",
        "java.io.FilterReader",
        "java.io.FilterWriter",
        "java.util.InputMismatchException",
    };

    static void row(String label, String[] names, boolean init) {
        long[] t = new long[names.length];
        int found = 0;
        long total = 0;
        for (int i = 0; i < names.length; i++) {
            long t0 = System.nanoTime();
            try {
                Class.forName(names[i], init, null);
                found++;
            } catch (ClassNotFoundException e) {
                // counted out of the checksum
            }
            t[i] = System.nanoTime() - t0;
            total += t[i];
        }
        long[] c = t.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        double med = c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", label, (double) c[0], med);
        System.out.println("total " + label + " " + total + " ns");
        System.out.println("checksum " + label + " " + found);
    }

    public static void main(String[] args) {
        row("boot-load", LOAD, false);
        row("boot-load-init", INIT, true);
    }
}
