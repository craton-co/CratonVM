import java.io.*;
import java.lang.management.*;
import java.lang.reflect.*;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.*;

/**
 * nio21 wave 2 lane L4: conformance and bench rows for the six `--compatible` rows the lane
 * added on the `HttpClient` request path, all default ON:
 *
 *  * `jdk-tree-map`              TreeMap / TreeSet (the `HttpHeaders` case-insensitive maps)
 *  * `jdk-linked-collections`    LinkedList / ArrayDeque
 *  * `jdk-completable-future`    CompletableFuture's retired rows (complete, allOf, ...)
 *  * `jdk-time-unit`             TimeUnit's nine conversions
 *  * `jdk-adders`                LongAdder / DoubleAdder
 *  * `jdk-carrier-thread-locals` the per-thread carrier-thread-local accessors and the
 *                                `sun.nio.ch.Util` temporary-buffer pool (row `ctl`: many
 *                                threads doing gathering / positional FileChannel I/O, which
 *                                share `IOVecWrapper` / `Util$BufferCache` over the old map)
 *
 * plus `aqs` (ThreadMXBean locked synchronizers: the `setExclusiveOwnerThread` index that
 * `CRATONVM_AOS_OWNER_INDEX=0` turns off -- that arm is EXPECTED to differ on `aqs` only).
 *
 * Every conformance line is a fact (sizes, orders, results, exception types and messages),
 * so the output diffs against HotSpot 25.
 *
 * Run:
 *   javac -d out tools/probes/Nio21HttpFamilies.java
 *   java -cp out Nio21HttpFamilies > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21HttpFamilies > jo.txt
 *   cratonvm --compatible -cp out Nio21HttpFamilies > co.txt
 *   CRATONVM_COMPAT_ALIGN=-jdk-tree-map,-jdk-linked-collections,-jdk-completable-future,-jdk-time-unit,-jdk-adders,-jdk-carrier-thread-locals \
 *       cratonvm --compatible -cp out Nio21HttpFamilies > co-off.txt
 * hs, jo and co must be identical; co-off is the old `--compatible` (the diff is what the rows
 * fixed, if anything). -Dnio21.rows=a,b selects rows.
 *
 * Bench (`<row> <N> ns/op`): -Dnio21.bench=true, rows bench-tmap bench-ll bench-ad bench-cf
 * bench-tu bench-la bench-lock (-Dnio21.iters=N default 200000, -Dnio21.warm=N default 3).
 * Interleave `--jdk-only`, `--compatible` and the `co-off` arm, 5 runs each.
 */
public class Nio21HttpFamilies {
    static String rows = System.getProperty("nio21.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        if (Boolean.getBoolean("nio21.bench")) { bench(); return; }
        String[] all = {"tree", "linked", "cf", "timeunit", "adders", "ctl", "aqs"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio21HttpFamilies.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    static void p(String s) { System.out.println(s); }

    static void tree() {
        TreeMap<String, List<String>> h = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
        h.put("Content-Type", List.of("text/plain"));
        h.put("content-length", List.of("12"));
        h.put("X-B", List.of("1", "2"));
        h.computeIfAbsent("x-a", k -> new ArrayList<>()).add("v");
        h.merge("CONTENT-LENGTH", List.of("13"), (o, n) -> n);
        p("tree map " + h + " size=" + h.size());
        p("tree get " + h.get("CONTENT-TYPE") + " " + h.get("nope") + " " + h.containsKey("x-b"));
        p("tree nav " + h.firstKey() + " " + h.lastKey() + " " + h.ceilingKey("d") + " "
                + h.floorKey("d") + " " + h.higherKey("x-a") + " " + h.lowerKey("Content-Type"));
        p("tree views " + h.keySet() + " " + h.values() + " " + h.descendingMap().keySet()
                + " " + h.headMap("x").keySet() + " " + h.tailMap("x", true).keySet());
        Iterator<Map.Entry<String, List<String>>> it = h.entrySet().iterator();
        it.next();
        it.remove();
        p("tree iter-remove " + h.keySet() + " " + h.pollFirstEntry() + " " + h.pollLastEntry());
        try {
            for (String k : h.keySet()) h.put(k + "!", List.of());
            p("tree cme none");
        } catch (Throwable t) { p("tree cme " + t.getClass().getName()); }
        try { new TreeMap<Object, Object>().put(null, 1); } catch (Throwable t) { p("tree null " + t.getClass().getName()); }
        try { new TreeMap<Object, Object>().put(new Object(), 1); new TreeMap<Object, Object>(Map.of(1, 1, "a", 2)); }
        catch (Throwable t) { p("tree cce " + t.getClass().getName()); }
        TreeSet<String> s = new TreeSet<>(Comparator.reverseOrder());
        Collections.addAll(s, "b", "a", "d", "c");
        p("tree set " + s + " " + s.isEmpty() + " " + s.first() + " " + s.pollLast() + " " + s.headSet("b")
                + " " + s.descendingSet() + " " + s.comparator().compare("a", "b"));
        TreeMap<Integer, Integer> big = new TreeMap<>();
        for (int i = 0; i < 1000; i++) big.put((i * 7919) % 1000, i);
        long sum = 0;
        for (Map.Entry<Integer, Integer> e : big.entrySet()) sum = sum * 31 + e.getKey() * 3 + e.getValue();
        p("tree big " + big.size() + " " + sum + " " + big.subMap(10, 20).size() + " " + new ArrayList<>(big.keySet()).get(500));
        p("tree classes " + h.getClass().getName() + " " + h.keySet().getClass().getName() + " "
                + h.entrySet().iterator().getClass().getName() + " " + s.iterator().getClass().getName());
    }

    static void linked() {
        LinkedList<String> l = new LinkedList<>(List.of("a", "b", "c"));
        l.addFirst("z");
        l.add(2, "m");
        l.removeLastOccurrence("c");
        p("linked list " + l + " " + l.size() + " " + l.isEmpty() + " " + l.peekLast() + " " + l.indexOf("m"));
        ListIterator<String> li = l.listIterator(1);
        li.next();
        li.set("A");
        li.add("q");
        li.previous();
        li.remove();
        p("linked itr " + l + " " + li.nextIndex() + " " + li.getClass().getName() + " " + l.iterator().getClass().getName());
        try { for (String s : l) if (s.equals("A")) l.add("x"); p("linked cme none"); }
        catch (Throwable t) { p("linked cme " + t.getClass().getName()); }
        l.add(null);
        p("linked null " + l + " " + l.pollFirst() + " " + l.reversed());
        try { new LinkedList<>().removeFirst(); } catch (Throwable t) { p("linked empty " + ex(t)); }
        ArrayDeque<Integer> d = new ArrayDeque<>(2);
        for (int i = 0; i < 20; i++) { if (i % 3 == 0) d.offerFirst(i); else d.offerLast(i); }
        for (int i = 0; i < 5; i++) d.pollFirst();
        d.removeIf(x -> x % 4 == 0);
        p("deque " + d + " " + d.size() + " " + d.peekFirst() + " " + d.peekLast());
        StringBuilder sb = new StringBuilder();
        for (Iterator<Integer> it = d.descendingIterator(); it.hasNext(); ) { int x = it.next(); sb.append(x).append(','); if (x == 8) it.remove(); }
        p("deque desc " + sb + " " + d + " " + d.size());
        try { d.add(null); } catch (Throwable t) { p("deque null " + ex(t)); }
        try { new ArrayDeque<>().pop(); } catch (Throwable t) { p("deque empty " + t.getClass().getName()); }
        Queue<String> q = new LinkedList<>();
        q.offer(null);
        q.offer("x");
        p("queue linked " + q.size() + " " + q.poll() + " " + q.peek());
        Deque<String> dq = new ArrayDeque<>();
        dq.push("a"); dq.push("b");
        p("deque iface " + dq.pop() + " " + dq.size() + " " + dq.isEmpty());
    }

    static void cf() throws Exception {
        CompletableFuture<String> f = new CompletableFuture<>();
        p("cf first " + f.complete("a") + " " + f.complete("b") + " " + f.join() + " " + f.isCompletedExceptionally());
        CompletableFuture<String> g = new CompletableFuture<>();
        p("cf exc " + g.completeExceptionally(new IllegalStateException("boom")) + " " + g.complete("x")
                + " " + g.isCompletedExceptionally() + " " + g.isDone());
        try { g.join(); } catch (CompletionException e) { p("cf join " + ex(e) + " / " + ex(e.getCause())); }
        try { new CompletableFuture<String>().completeExceptionally(null); } catch (Throwable t) { p("cf npe " + t.getClass().getName()); }
        CompletableFuture<Integer> x = CompletableFuture.supplyAsync(() -> 20);
        CompletableFuture<Integer> y = CompletableFuture.supplyAsync(() -> 22);
        p("cf combine " + x.thenCombine(y, Integer::sum).get(5, TimeUnit.SECONDS));
        CompletableFuture<Void> all = CompletableFuture.allOf(x, y, CompletableFuture.completedFuture(1));
        p("cf allOf " + all.get(5, TimeUnit.SECONDS) + " " + all.isDone());
        p("cf anyOf " + CompletableFuture.anyOf(CompletableFuture.completedFuture("k"), new CompletableFuture<>()).get());
        try { x.thenCombine(null, (u, v) -> u); } catch (Throwable t) { p("cf combine-null " + t.getClass().getName()); }
        // Two racing completers: exactly one wins.
        int wins = 0;
        for (int i = 0; i < 200; i++) {
            CompletableFuture<Integer> r = new CompletableFuture<>();
            AtomicInteger w = new AtomicInteger();
            Thread t1 = new Thread(() -> { if (r.complete(1)) w.incrementAndGet(); });
            Thread t2 = new Thread(() -> { if (r.completeExceptionally(new RuntimeException())) w.incrementAndGet(); });
            t1.start(); t2.start(); t1.join(); t2.join();
            if (w.get() == 1) wins++;
        }
        p("cf race single-winner " + wins + "/200");
        CompletableFuture<String> chain = new CompletableFuture<>();
        CompletableFuture<String> dep = chain.thenApply(s -> s + "!").thenApplyAsync(s -> s + "?");
        chain.complete("go");
        p("cf chain " + dep.get(5, TimeUnit.SECONDS) + " " + f.toString().replaceAll("@[0-9a-f]+", "@X"));
    }

    static void timeunit() throws Exception {
        StringBuilder sb = new StringBuilder("tu");
        long[] vs = {0, 1, -1, 999, 1_000_001, Long.MAX_VALUE, Long.MIN_VALUE, Long.MAX_VALUE / 1000};
        for (TimeUnit u : TimeUnit.values()) {
            for (long v : vs) {
                sb.append(' ').append(u.toNanos(v)).append('/').append(u.toMicros(v)).append('/')
                  .append(u.toMillis(v)).append('/').append(u.toSeconds(v)).append('/')
                  .append(u.toMinutes(v)).append('/').append(u.toHours(v)).append('/').append(u.toDays(v));
            }
        }
        p(sb.toString());
        sb.setLength(0);
        for (TimeUnit s : TimeUnit.values()) for (TimeUnit t : TimeUnit.values()) sb.append(t.convert(3, s)).append(',');
        p("tu convert " + sb);
        p("tu duration " + TimeUnit.SECONDS.convert(java.time.Duration.ofMillis(2500)) + " " + TimeUnit.of(java.time.temporal.ChronoUnit.HOURS));
        long t0 = System.nanoTime();
        TimeUnit.MILLISECONDS.sleep(5);
        p("tu sleep>=5ms " + (System.nanoTime() - t0 >= 5_000_000L));
        TimeUnit.MILLISECONDS.sleep(-1);
        p("tu sleep-negative ok");
    }

    static void adders() throws Exception {
        LongAdder la = new LongAdder();
        DoubleAdder da = new DoubleAdder();
        Thread[] ts = new Thread[8];
        for (int i = 0; i < ts.length; i++) {
            ts[i] = new Thread(() -> { for (int k = 0; k < 20000; k++) { la.increment(); la.add(2); da.add(0.5); } la.decrement(); });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        p("adders sum " + la.sum() + " " + la.longValue() + " " + la.intValue() + " " + la + " " + da.sum());
        p("adders reset " + la.sumThenReset() + " " + la.sum());
        la.add(5);
        da.reset();
        p("adders after " + la.sum() + " " + da.sum() + " " + da.doubleValue());
    }

    static void ctl() throws Exception {
        Path dir = Files.createTempDirectory("nio21ctl");
        int threads = 8, rounds = 300;
        AtomicInteger bad = new AtomicInteger();
        AtomicReference<String> first = new AtomicReference<>();
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            final int id = i;
            ts[i] = new Thread(() -> {
                try (FileChannel ch = FileChannel.open(dir.resolve("f" + id),
                        StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    for (int r = 0; r < rounds; r++) {
                        String s1 = "t" + id + "r" + r + ":", s2 = "body-" + id + "-" + r + ";";
                        ByteBuffer[] bufs = {ByteBuffer.wrap(s1.getBytes(StandardCharsets.US_ASCII)),
                                ByteBuffer.wrap(s2.getBytes(StandardCharsets.US_ASCII))};
                        ch.truncate(0);
                        ch.position(0);
                        long n = ch.write(bufs);
                        ByteBuffer back = ByteBuffer.allocate((int) n);
                        ch.read(back, 0);
                        String got = new String(back.array(), StandardCharsets.US_ASCII);
                        if (!got.equals(s1 + s2)) { bad.incrementAndGet(); first.compareAndSet(null, got); }
                    }
                } catch (Throwable t) { bad.incrementAndGet(); first.compareAndSet(null, ex(t)); }
            });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        p("ctl gathering+positional failures=" + bad.get() + (first.get() == null ? "" : " first=" + first.get()));
        for (int i = 0; i < threads; i++) Files.deleteIfExists(dir.resolve("f" + i));
        Files.deleteIfExists(dir);
    }

    static void aqs() {
        ReentrantLock lock = new ReentrantLock();
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        lock.lock();
        rw.writeLock().lock();
        try {
            ThreadMXBean mx = ManagementFactory.getThreadMXBean();
            p("aqs supported " + mx.isSynchronizerUsageSupported());
            ThreadInfo ti = mx.getThreadInfo(new long[] {Thread.currentThread().getId()}, false, true)[0];
            List<String> names = new ArrayList<>();
            for (LockInfo li : ti.getLockedSynchronizers()) names.add(li.getClassName());
            Collections.sort(names);
            p("aqs locked " + names);
        } finally {
            rw.writeLock().unlock();
            lock.unlock();
        }
        ThreadInfo after = ManagementFactory.getThreadMXBean()
                .getThreadInfo(new long[] {Thread.currentThread().getId()}, false, true)[0];
        p("aqs after-unlock " + after.getLockedSynchronizers().length + " " + lock.isLocked() + " " + lock.getHoldCount());
    }

    // ---------------------------------------------------------------- bench

    static volatile Object sink;

    static void bench() throws Exception {
        int iters = Integer.getInteger("nio21.iters", 200000);
        int warm = Integer.getInteger("nio21.warm", 3);
        String[] all = {"bench-tmap", "bench-ll", "bench-ad", "bench-cf", "bench-tu", "bench-la", "bench-lock"};
        for (String r : all) {
            if (!rows.equals("all") && !on(r)) continue;
            long best = Long.MAX_VALUE;
            for (int w = 0; w <= warm; w++) {
                long t0 = System.nanoTime();
                run(r, iters);
                long dt = System.nanoTime() - t0;
                if (w > 0 || warm == 0) best = Math.min(best, dt);
            }
            System.out.println(r + " " + (best / iters) + " ns/op");
        }
    }

    static final String[] KEYS = {"Content-Type", "content-length", "Host", "User-Agent", "Accept", "Connection"};

    static void run(String r, int n) throws Exception {
        switch (r) {
            case "bench-tmap": {
                for (int i = 0; i < n; i++) {
                    TreeMap<String, String> m = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
                    for (String k : KEYS) m.put(k, k);
                    sink = m.get("HOST");
                }
                break;
            }
            case "bench-ll": {
                LinkedList<Integer> l = new LinkedList<>();
                for (int i = 0; i < n; i++) { l.add(i); if (!l.isEmpty() && l.size() > 4) sink = l.poll(); }
                break;
            }
            case "bench-ad": {
                ArrayDeque<Integer> d = new ArrayDeque<>();
                for (int i = 0; i < n; i++) { d.offerLast(i); if (d.size() > 4) sink = d.pollFirst(); }
                break;
            }
            case "bench-cf": {
                for (int i = 0; i < n; i++) { CompletableFuture<Integer> f = new CompletableFuture<>(); f.complete(i); sink = f.join(); }
                break;
            }
            case "bench-tu": {
                long acc = 0;
                for (int i = 0; i < n; i++) acc += TimeUnit.MILLISECONDS.toNanos(i) + TimeUnit.NANOSECONDS.toMillis(i);
                sink = acc;
                break;
            }
            case "bench-la": {
                LongAdder la = new LongAdder();
                for (int i = 0; i < n; i++) la.add(i);
                sink = la.sum();
                break;
            }
            case "bench-lock": {
                ReentrantLock lock = new ReentrantLock();
                for (int i = 0; i < n; i++) { lock.lock(); try { sink = lock; } finally { lock.unlock(); } }
                break;
            }
            default:
                throw new IllegalArgumentException(r);
        }
    }
}
