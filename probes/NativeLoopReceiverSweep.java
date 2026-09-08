// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The loop-carried stale receiver, exercised.
//
// `docs/internal/audits/natives-loop-carried-stale-receivers-RETIRED-20260907.md`
// and its sibling on laundered refresh contracts name ~40 native sites where a
// reference was carried across a collection: a loop body that dispatches Java
// on the SAME receiver every turn, or a helper that took its receiver by value
// in front of an API whose `&mut` promised to refresh it. Every one of them is
// reached from ordinary library code; this sweep is that code.
//
// It is a CONSISTENCY probe, not a crash probe. Every printed line is chosen by
// the program -- a count, a sorted join, a boolean -- so HotSpot is a usable
// oracle and a diff is a defect. A stale receiver does not usually SIGSEGV: it
// reads a zeroed or forwarded-from header and answers a wrong size, an empty
// collection, or a null field, and those are exactly what these lines are.
//
// The pressure matters more than the iteration count. Each section allocates
// garbage between turns and calls `System.gc()` around the native, so a moving
// young collection has a chance to land inside the loop rather than between
// two of them.
//
//   javac -d out NativeLoopReceiverSweep.java
//   java                -cp out NativeLoopReceiverSweep   # oracle
//   cratonvm            -cp out NativeLoopReceiverSweep   # and the VM
//
// THE CONFIGURATION THAT SEPARATES THE TWO BINARIES, measured 2026-09-08:
//
//   CRATONVM_DBG_GC_STRESS=65536 cratonvm --XX:UseGc Generational -Xmx256m \
//       -cp out NativeLoopReceiverSweep annotated
//
// answers `annotatedParameterTypes.total = 17` (expected 20) on `origin/dev`
// at ff636f3c1, 0 of 5 runs correct, and 5 of 5 correct once
// `native_executable_get_annotated_parameter_types` pins the mirrors it walks.
// A stress interval of 1 MB never reproduces and 256 KB always does: the
// window is a few hundred bytes of allocation wide, which is why an ordinary
// workload's A/B on this family is flat. Add `CRATONVM_DBG_FORCE_MOVING=1
// CRATONVM_DBG_STALE_OBJREF=1` and the same defect is a SIGSEGV instead of a
// wrong answer. G1 is unaffected.
//
// Sections can be named on the command line -- `.. NativeLoopReceiverSweep
// growth annotated` -- so a failure can be isolated from the one before it.
// One caveat, with its own page: the `growth` section SIGSEGVs under all THREE
// of those flags together, identically on both binaries and at the same minor
// cycle every run. See
// `docs/internal/audits/stale-value-at-set_field-methodhandles-lookup-RETIRED-20260908.md`.
// It was `MethodHandles.lookup()` storing a pre-GC class mirror, reached from
// `ConcurrentSkipListMap.<clinit>`, and it is fixed -- the caveat is kept here
// because the section is what found it.

import java.io.StringReader;
import java.io.StringWriter;
import java.lang.reflect.Method;
import java.nio.file.attribute.PosixFilePermission;
import java.nio.file.attribute.PosixFilePermissions;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;
import java.util.Map;
import java.util.PriorityQueue;
import java.util.Properties;
import java.util.Set;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentSkipListMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.Exchanger;
import java.util.concurrent.ExecutorCompletionService;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.Stream;

public class NativeLoopReceiverSweep {

    /** Allocation churn: enough to move a young generation, cheap enough to loop on. */
    private static Object sink;

    private static void churn(int n) {
        for (int i = 0; i < n; i++) {
            sink = new byte[256];
            sink = new String(new char[32]);
        }
    }

    private static int failures = 0;

    private static void check(String name, Object expected, Object actual) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        System.out.println((ok ? "OK   " : "FAIL ") + name + " = " + actual);
        if (!ok) {
            System.out.println("     expected " + expected);
            failures++;
        }
    }

    // ------------------------------------------------------------------ 1
    // `mirror_loaded_entries_to_properties_backend` and `collect_store_entries`:
    // a `for (k, v) in parsed` loop that allocates two strings and dispatches a
    // virtual `put` on a receiver read once, before the loop.
    private static void properties() throws Exception {
        StringBuilder src = new StringBuilder();
        for (int i = 0; i < 200; i++) {
            src.append("key").append(i).append("=value").append(i).append('\n');
        }
        Properties p = new Properties();
        churn(400);
        p.load(new StringReader(src.toString()));
        System.gc();
        check("properties.size", 200, p.size());
        check("properties.get(key0)", "value0", p.getProperty("key0"));
        check("properties.get(key199)", "value199", p.getProperty("key199"));

        int names = 0;
        for (Iterator<?> it = p.propertyNames().asIterator(); it.hasNext(); it.next()) {
            names++;
            if ((names & 31) == 0) {
                churn(50);
            }
        }
        check("properties.propertyNames.count", 200, names);

        // `collect_store_entries` -> `ordered_snapshot_kv(&mut this)`: the
        // contract the by-value wrapper used to launder.
        StringWriter out = new StringWriter();
        churn(200);
        p.store(out, null);
        long lines = out.toString().lines().filter(l -> l.startsWith("key")).count();
        check("properties.store.lines", 200L, lines);

        // A SUBCLASS takes the virtual-entrySet arm of the same helper.
        Properties sub = new Properties() {};
        sub.putAll(p);
        StringWriter subOut = new StringWriter();
        churn(200);
        sub.store(subOut, null);
        check("properties.subclass.store.lines", 200L,
                subOut.toString().lines().filter(l -> l.startsWith("key")).count());
    }

    // ------------------------------------------------------------------ 2
    // `stream_elements` and friends: the receiver an 85-call-site helper used
    // to refresh into its own copy.
    private static void streams() throws Exception {
        List<Integer> src = new ArrayList<>();
        for (int i = 0; i < 300; i++) {
            src.add(i);
        }
        churn(300);
        Stream<Integer> s = src.stream();
        System.gc();
        check("stream.count", 300L, s.count());

        churn(300);
        check("stream.sum", 44850, src.stream().mapToInt(Integer::intValue).sum());
        churn(300);
        check("stream.filter.count", 150L, src.stream().filter(i -> (i & 1) == 0).count());
        churn(300);
        check("stream.anyMatch", Boolean.TRUE, src.stream().anyMatch(i -> i == 299));
        churn(300);
        check("stream.allMatch", Boolean.TRUE, src.stream().allMatch(i -> i >= 0));
        churn(300);
        check("stream.noneMatch", Boolean.TRUE, src.stream().noneMatch(i -> i < 0));
        churn(300);
        check("stream.concat.count", 600L,
                Stream.concat(src.stream(), src.stream()).count());
        churn(300);
        check("stream.flatMap.count", 900L,
                src.stream().flatMap(i -> Stream.of(i, i, i)).count());
        churn(300);
        check("stream.collect.joining.len", 1089,
                src.stream().map(String::valueOf).collect(Collectors.joining(",")).length());
        churn(300);
        check("intstream.boxed.count", 300L, IntStream.range(0, 300).boxed().count());
        churn(300);
        check("intstream.average", 149.5,
                IntStream.range(0, 300).average().orElse(-1.0));
        churn(300);
        check("stream.sorted.first", 0, src.stream().sorted().findFirst().orElse(-1));
    }

    // ------------------------------------------------------------------ 3
    // `native_p64_ll_reversed`, `native_p64_lhm_reversed`,
    // `p64_seq_map_edge_entry`: each walks a collection with a virtual dispatch
    // per element on a receiver bound before the loop.
    private static void sequenced() throws Exception {
        List<String> al = new ArrayList<>();
        for (int i = 0; i < 200; i++) {
            al.add("e" + i);
        }
        churn(400);
        List<String> rev = al.reversed();
        System.gc();
        check("list.reversed.size", 200, rev.size());
        check("list.reversed.first", "e199", rev.get(0));
        check("list.reversed.last", "e0", rev.get(199));

        LinkedHashMap<String, String> lhm = new LinkedHashMap<>();
        for (int i = 0; i < 200; i++) {
            lhm.put("k" + i, "v" + i);
        }
        churn(400);
        Map<String, String> lrev = lhm.reversed();
        System.gc();
        check("lhm.reversed.size", 200, lrev.size());
        check("lhm.reversed.firstKey", "k199", lrev.keySet().iterator().next());
        churn(400);
        check("lhm.firstEntry", "k0=v0", String.valueOf(lhm.firstEntry()));
        churn(400);
        check("lhm.lastEntry", "k199=v199", String.valueOf(lhm.lastEntry()));

        LinkedList<String> ll = new LinkedList<>(al);
        churn(400);
        check("linkedlist.reversed.first", "e199", ll.reversed().get(0));

        int seen = 0;
        StringBuilder acc = new StringBuilder();
        for (Map.Entry<String, String> e : lhm.entrySet()) {
            acc.append(e.getKey());
            if ((++seen & 15) == 0) {
                churn(60);
            }
        }
        check("lhm.entrySet.walked", 200, seen);
        check("lhm.entrySet.acc.len", 690, acc.length());
    }

    // ------------------------------------------------------------------ 4
    // The `_ensure_capacity` family: every one of these grows a backing array
    // inside a helper that used to refresh only its own copy of the receiver.
    private static void growth() throws Exception {
        ConcurrentSkipListMap<String, String> cslm = new ConcurrentSkipListMap<>();
        for (int i = 0; i < 300; i++) {
            cslm.put(String.format("k%04d", i), "v" + i);
            if ((i & 31) == 0) {
                churn(80);
            }
        }
        System.gc();
        check("cslm.size", 300, cslm.size());
        check("cslm.firstKey", "k0000", cslm.firstKey());
        check("cslm.lastKey", "k0299", cslm.lastKey());
        check("cslm.get(k0150)", "v150", cslm.get("k0150"));

        CopyOnWriteArrayList<String> cowal = new CopyOnWriteArrayList<>();
        for (int i = 0; i < 200; i++) {
            cowal.addIfAbsent("c" + i);
            if ((i & 31) == 0) {
                churn(80);
            }
        }
        churn(200);
        check("cowal.size", 200, cowal.size());
        check("cowal.contains(c199)", Boolean.TRUE, cowal.contains("c199"));
        check("cowal.addAll", Boolean.TRUE, cowal.addAll(List.of("x", "y")));
        check("cowal.size.after", 202, cowal.size());

        LinkedBlockingQueue<String> lbq = new LinkedBlockingQueue<>();
        for (int i = 0; i < 300; i++) {
            lbq.offer("q" + i);
            if ((i & 31) == 0) {
                churn(80);
            }
        }
        System.gc();
        check("lbq.size", 300, lbq.size());
        check("lbq.peek", "q0", lbq.peek());

        LinkedBlockingDeque<String> lbd = new LinkedBlockingDeque<>();
        for (int i = 0; i < 200; i++) {
            lbd.offerFirst("d" + i);
            if ((i & 31) == 0) {
                churn(80);
            }
        }
        System.gc();
        check("lbd.size", 200, lbd.size());
        check("lbd.peekFirst", "d199", lbd.peekFirst());

        ArrayDeque<String> ad = new ArrayDeque<>();
        for (int i = 0; i < 400; i++) {
            ad.addLast("a" + i);
            if ((i & 31) == 0) {
                churn(60);
            }
        }
        System.gc();
        check("arraydeque.size", 400, ad.size());
        check("arraydeque.peekFirst", "a0", ad.peekFirst());
        check("arraydeque.peekLast", "a399", ad.peekLast());

        PriorityQueue<Integer> pq = new PriorityQueue<>(Comparator.reverseOrder());
        for (int i = 0; i < 400; i++) {
            pq.add(i);
            if ((i & 31) == 0) {
                churn(60);
            }
        }
        System.gc();
        check("pq.size", 400, pq.size());
        check("pq.peek", 399, pq.peek());
    }

    // ------------------------------------------------------------------ 5
    // `lli_resnapshot`: the ListIterator re-snapshots the list inside a helper
    // that took the iterator by value.
    private static void listIterator() throws Exception {
        LinkedList<String> ll = new LinkedList<>();
        for (int i = 0; i < 120; i++) {
            ll.add("n" + i);
        }
        churn(400);
        ListIterator<String> it = ll.listIterator();
        int removed = 0;
        while (it.hasNext()) {
            String v = it.next();
            if (v.endsWith("0")) {
                it.remove();
                removed++;
                churn(40);
            }
        }
        System.gc();
        check("linkedlist.listItr.removed", 12, removed);
        check("linkedlist.size.after", 108, ll.size());

        ListIterator<String> add = ll.listIterator();
        int added = 0;
        while (add.hasNext()) {
            add.next();
            if ((added & 7) == 0) {
                add.add("z" + added);
                churn(40);
            }
            added++;
        }
        check("linkedlist.size.afterAdd", 108 + 14, ll.size());
    }

    // ------------------------------------------------------------------ 6
    // `posix_permission_bits_from_set`: `Set.contains` on a receiver bound
    // before a nine-turn loop.
    private static void posixPermissions() throws Exception {
        Set<PosixFilePermission> perms = new TreeSet<>(List.of(
                PosixFilePermission.OWNER_READ,
                PosixFilePermission.OWNER_WRITE,
                PosixFilePermission.GROUP_READ,
                PosixFilePermission.OTHERS_READ));
        churn(400);
        System.gc();
        check("posix.toString", "rw-r--r--", PosixFilePermissions.toString(perms));
        churn(400);
        check("posix.roundTrip", perms.toString(),
                new TreeSet<>(PosixFilePermissions.fromString("rw-r--r--")).toString());
    }

    // ------------------------------------------------------------------ 7
    // `exchanger_do_exchange`'s INNER loop, which parks on `monitor_wait` while
    // a peer thread's collection runs.
    private static void exchanger() throws Exception {
        Exchanger<String> ex = new Exchanger<>();
        StringBuilder got = new StringBuilder();
        Thread peer = new Thread(() -> {
            try {
                for (int i = 0; i < 20; i++) {
                    churn(200);
                    got.append(ex.exchange("b" + i));
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        peer.start();
        StringBuilder mine = new StringBuilder();
        for (int i = 0; i < 20; i++) {
            churn(200);
            mine.append(ex.exchange("a" + i));
        }
        peer.join();
        check("exchanger.mine.len", 50, mine.length());
        check("exchanger.peer.len", 50, got.length());
        check("exchanger.mine.head", "b0b1b2", mine.substring(0, 6));
        check("exchanger.peer.head", "a0a1a2", got.substring(0, 6));
    }

    // ------------------------------------------------------------------ 8
    // `ecs_take` / `ecs_poll_timed`: a poll loop around a blocking region.
    private static void completionService() throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(2);
        try {
            ExecutorCompletionService<Integer> ecs = new ExecutorCompletionService<>(pool);
            for (int i = 0; i < 20; i++) {
                final int n = i;
                ecs.submit(() -> {
                    churn(100);
                    return n * n;
                });
            }
            long total = 0;
            for (int i = 0; i < 20; i++) {
                churn(150);
                total += ecs.take().get();
            }
            check("completionService.total", 2470L, total);
        } finally {
            pool.shutdown();
        }
    }

    // ------------------------------------------------------------------ 9
    // `native_executable_get_annotated_parameter_types`: the output array and
    // every parameter mirror are carried across a per-parameter allocation.
    private static void annotatedParameterTypes() throws Exception {
        int total = 0;
        for (Method m : Sample.class.getDeclaredMethods()) {
            churn(200);
            total += m.getAnnotatedParameterTypes().length;
            churn(200);
            total += m.getGenericParameterTypes().length;
        }
        System.gc();
        check("annotatedParameterTypes.total", 20, total);
    }

    static class Sample {
        void a(String x, int y, List<String> z) {}

        void b(Map<String, List<Integer>> m, String[] s) {}

        void c(int a, int b, int c, int d, int e) {}
    }

    /** Named sections, so a failure can be isolated from the one before it. */
    interface Section {
        void run() throws Exception;
    }

    private static final String[] NAMES = {
        "properties", "streams", "sequenced", "growth", "listIterator",
        "posix", "exchanger", "completionService", "annotated",
    };

    public static void main(String[] args) throws Exception {
        Section[] sections = {
            NativeLoopReceiverSweep::properties,
            NativeLoopReceiverSweep::streams,
            NativeLoopReceiverSweep::sequenced,
            NativeLoopReceiverSweep::growth,
            NativeLoopReceiverSweep::listIterator,
            NativeLoopReceiverSweep::posixPermissions,
            NativeLoopReceiverSweep::exchanger,
            NativeLoopReceiverSweep::completionService,
            NativeLoopReceiverSweep::annotatedParameterTypes,
        };
        List<String> want = args.length == 0 ? List.of(NAMES) : Arrays.asList(args);
        for (int i = 0; i < NAMES.length; i++) {
            if (want.contains(NAMES[i])) {
                sections[i].run();
            }
        }
        System.out.println(failures == 0
                ? "PASS NativeLoopReceiverSweep"
                : "DEFECT NativeLoopReceiverSweep failures=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }
}
