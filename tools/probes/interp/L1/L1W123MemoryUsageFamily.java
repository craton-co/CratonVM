// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L1 -- the flip-rule probe and bench for
// the `JdkMemoryUsage` compat family (alignment bit 96,
// `CRATONVM_COMPAT_JDK_MEMORY_USAGE`, default off): with the row on,
// `--compatible` runs the JDK's own `MemoryUsage` constructor, getters and
// toString, as `--jdk-only` does. Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// Covers: a Java-built usage (getters, toString, equals/hashCode are Object's),
// every IllegalArgumentException the constructor throws (messages), the
// CompositeData round trip (MemoryUsage.from), and VM-built usages
// (MemoryMXBean heap / non-heap, every pool's usage / peak / collection usage)
// by their invariants only, since the numbers are the VM's. TIME lines: the
// getters on a Java-built and on a VM-built usage, and the constructor.
//
// Must be SAME in all three arms (TIME lines filtered):
//   javac -d out L1W123MemoryUsageFamily.java
//   java -cp out L1W123MemoryUsageFamily
//   cratonvm --jdk-only -cp out L1W123MemoryUsageFamily
//   cratonvm --compatible -cp out L1W123MemoryUsageFamily
//   CRATONVM_COMPAT_JDK_MEMORY_USAGE=1 cratonvm --compatible -cp out L1W123MemoryUsageFamily
//   (each also with --nojit for the interpreter's TIME rows)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// built 1024/2048/4096/8192
// built toString=[init = 1024(1K) used = 2048(2K) committed = 4096(4K) max = 8192(8K)]
// built class=java.lang.management.MemoryUsage final=false
// undefined ok -1/0/0/-1 [init = -1(-1K) used = 0(0K) committed = 0(0K) max = -1(-1K)]
// kib ok 1048576/3145728/5242880/7340032 [init = 1048576(1024K) used = 3145728(3072K) committed = 5242880(5120K) max = 7340032(7168K)]
// init<-1 -> java.lang.IllegalArgumentException: [init parameter = -2 is negative but not -1.]
// used<0 -> java.lang.IllegalArgumentException: [used parameter = -1 is negative.]
// committed<0 -> java.lang.IllegalArgumentException: [committed parameter = -1 is negative.]
// max<-1 -> java.lang.IllegalArgumentException: [max parameter = -2 is negative but not -1.]
// used>committed -> java.lang.IllegalArgumentException: [used = 10 should be <= committed = 5]
// committed>max -> java.lang.IllegalArgumentException: [committed = 10 should be < max = 8]
// long-max ok 9223372036854775807/9223372036854775807/9223372036854775807/9223372036854775807 [init = 9223372036854775807(9007199254740991K) used = 9223372036854775807(9007199254740991K) committed = 9223372036854775807(9007199254740991K) max = 9223372036854775807(9007199254740991K)]
// from(cd) 1024/2048/4096/8192
// from(null)=null
// heap class=java.lang.management.MemoryUsage invariants=true toString-format=true
// nonheap class=java.lang.management.MemoryUsage invariants=true toString-format=true
// pools-nonempty=true pools-ok=true
// done
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.util.Arrays;
import javax.management.openmbean.CompositeData;

public class L1W123MemoryUsageFamily {
    static volatile long sink;

    static void ctor(String label, long init, long used, long committed, long max) {
        try {
            MemoryUsage u = new MemoryUsage(init, used, committed, max);
            System.out.println(label + " ok " + u.getInit() + "/" + u.getUsed() + "/" + u.getCommitted()
                    + "/" + u.getMax() + " [" + u + "]");
        } catch (Throwable t) {
            System.out.println(label + " -> " + t.getClass().getName() + ": [" + t.getMessage() + "]");
        }
    }

    /** The invariants HotSpot's own usages keep; numbers are not compared. */
    static String shape(MemoryUsage u) {
        if (u == null) return "null";
        boolean ok = u.getInit() >= -1 && u.getUsed() >= 0 && u.getCommitted() >= u.getUsed()
                && (u.getMax() == -1 || u.getCommitted() <= u.getMax());
        String s = u.toString();
        boolean fmt = s.matches("init = -?\\d+\\(-?\\d+K\\) used = \\d+\\(\\d+K\\) committed = \\d+\\(\\d+K\\) max = -?\\d+\\(-?\\d+K\\)");
        return "class=" + u.getClass().getName() + " invariants=" + ok + " toString-format=" + fmt;
    }

    static long timeGetters(MemoryUsage u, int ops) {
        long[] ns = new long[7];
        for (int r = 0; r < ns.length; r++) {
            long acc = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < ops; i++) {
                acc += u.getInit() + u.getUsed() + u.getCommitted() + u.getMax();
            }
            ns[r] = System.nanoTime() - t0;
            sink += acc;
        }
        Arrays.sort(ns);
        return ns[ns.length / 2] / ops;
    }

    public static void main(String[] args) throws Exception {
        MemoryUsage a = new MemoryUsage(1024, 2048, 4096, 8192);
        System.out.println("built " + a.getInit() + "/" + a.getUsed() + "/" + a.getCommitted() + "/" + a.getMax());
        System.out.println("built toString=[" + a + "]");
        System.out.println("built class=" + a.getClass().getName() + " final="
                + java.lang.reflect.Modifier.isFinal(MemoryUsage.class.getModifiers()));
        ctor("undefined", -1, 0, 0, -1);
        ctor("kib", 1L << 20, 3L << 20, 5L << 20, 7L << 20);
        ctor("init<-1", -2, 0, 0, -1);
        ctor("used<0", 0, -1, 0, -1);
        ctor("committed<0", 0, 0, -1, -1);
        ctor("max<-1", 0, 0, 0, -2);
        ctor("used>committed", 0, 10, 5, -1);
        ctor("committed>max", 0, 5, 10, 8);
        ctor("long-max", Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE);

        // CompositeData round trip (the private MemoryUsage(CompositeData)).
        CompositeData cd = sun_management_toCompositeData(a);
        if (cd != null) {
            MemoryUsage back = MemoryUsage.from(cd);
            System.out.println("from(cd) " + back.getInit() + "/" + back.getUsed() + "/" + back.getCommitted()
                    + "/" + back.getMax());
        } else {
            System.out.println("from(cd) skipped");
        }
        System.out.println("from(null)=" + MemoryUsage.from(null));

        // VM-built usages.
        MemoryMXBean mem = ManagementFactory.getMemoryMXBean();
        System.out.println("heap " + shape(mem.getHeapMemoryUsage()));
        System.out.println("nonheap " + shape(mem.getNonHeapMemoryUsage()));
        boolean poolsOk = true;
        int pools = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            pools++;
            for (MemoryUsage u : new MemoryUsage[] { p.getUsage(), p.getPeakUsage(), p.getCollectionUsage() }) {
                if (u != null && !shape(u).endsWith("invariants=true toString-format=true")) {
                    poolsOk = false;
                    System.out.println("pool " + p.getName() + " bad " + shape(u) + " [" + u + "]");
                }
            }
        }
        System.out.println("pools-nonempty=" + (pools > 0) + " pools-ok=" + poolsOk);

        // TIME rows.
        System.out.println("TIME getters-java-built ns/4calls=" + timeGetters(a, 200_000));
        System.out.println("TIME getters-vm-built ns/4calls=" + timeGetters(mem.getHeapMemoryUsage(), 200_000));
        long[] ns = new long[7];
        for (int r = 0; r < ns.length; r++) {
            long acc = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < 100_000; i++) {
                acc += new MemoryUsage(i, i, i + 1, i + 2).getMax();
            }
            ns[r] = System.nanoTime() - t0;
            sink += acc;
        }
        Arrays.sort(ns);
        System.out.println("TIME ctor ns/op=" + ns[3] / 100_000);
        System.out.println("done");
    }

    /** MemoryUsage -> CompositeData of the MXBean mapping's shape, built by hand. */
    static CompositeData sun_management_toCompositeData(MemoryUsage u) {
        try {
            String[] names = { "committed", "init", "max", "used" };
            javax.management.openmbean.OpenType<?>[] types = {
                javax.management.openmbean.SimpleType.LONG, javax.management.openmbean.SimpleType.LONG,
                javax.management.openmbean.SimpleType.LONG, javax.management.openmbean.SimpleType.LONG,
            };
            javax.management.openmbean.CompositeType ct = new javax.management.openmbean.CompositeType(
                    MemoryUsage.class.getName(), MemoryUsage.class.getName(), names, names, types);
            Object[] values = { u.getCommitted(), u.getInit(), u.getMax(), u.getUsed() };
            return new javax.management.openmbean.CompositeDataSupport(ct, names, values);
        } catch (Throwable t) {
            System.out.println("cd build -> " + t.getClass().getName() + ": [" + t.getMessage() + "]");
            return null;
        }
    }
}
