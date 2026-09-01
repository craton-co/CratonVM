import java.util.*;
import java.util.concurrent.*;

/**
 * L6 differential sweep: the java.util.concurrent COLLECTIONS — the last
 * unswept part of this lane's package.
 *
 * The lane took ConcurrentHashMap, Thread, ForkJoinTask/Pool and
 * AsynchronousFileChannel; then the synchronizers; then the atomics. This is
 * the queues, the copy-on-write collections and the skip lists.
 *
 * Weighted at the NULL AXIS, BOUNDS and REFUSALS. Every one of these classes
 * refuses null where its java.util cousin accepts it, and that asymmetry is a
 * wrapper check — exactly what a native shadow drops.
 *
 * One row per assertion, never a nested print inside a row. Anything that can
 * block goes through `tw`, so a hang is a ROW and not a dead sweep.
 */
public class ConcurrentCollSweep {
    /** Drop `@<hex>` identity hashes. Hand-rolled: a probe must not normalise
     *  itself with machinery the VM under test also implements. */
    static String norm(String s) {
        StringBuilder b = new StringBuilder(s.length());
        int i = 0;
        while (i < s.length()) {
            char c = s.charAt(i);
            b.append(c);
            i++;
            if (c != '@') {
                continue;
            }
            int j = i;
            if (j + 1 < s.length() && s.charAt(j) == '0' && s.charAt(j + 1) == 'x') {
                j += 2;
            }
            int start = j;
            while (j < s.length()) {
                char h = s.charAt(j);
                boolean hex = (h >= '0' && h <= '9') || (h >= 'a' && h <= 'f') || (h >= 'A' && h <= 'F');
                if (!hex) {
                    break;
                }
                j++;
            }
            if (j > start) {
                b.append("<id>");
                i = j;
            }
        }
        return b.toString();
    }

    static void p(String tag, Object v) { System.out.println(tag + " |" + norm(String.valueOf(v)) + "|"); }

    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body c) {
        try { p(tag, c.run()); }
        catch (Throwable e) {
            String m = e.getMessage();
            p(tag, "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m));
        }
    }

    /** t() with a hard deadline: a blocking op that never returns is a ROW. */
    static void tw(String tag, Body c) {
        final Object[] out = new Object[1];
        Thread th = new Thread(() -> {
            try { out[0] = c.run(); }
            catch (Throwable e) {
                String m = e.getMessage();
                out[0] = "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m);
            }
        });
        th.setDaemon(true);
        th.start();
        try { th.join(4000); } catch (InterruptedException ignored) { }
        p(tag, th.isAlive() ? "TIMEOUT-4s" : String.valueOf(out[0]));
    }

    public static void main(String[] a) {
        // ---- ArrayBlockingQueue --------------------------------------------
        t("abq.ctor(0)", () -> new ArrayBlockingQueue<String>(0));
        t("abq.ctor(-1)", () -> new ArrayBlockingQueue<String>(-1));
        t("abq.remainingCapacity", () -> new ArrayBlockingQueue<String>(3).remainingCapacity());
        t("abq.offer(null)", () -> new ArrayBlockingQueue<String>(2).offer(null));
        t("abq.add(null)", () -> new ArrayBlockingQueue<String>(2).add(null));
        t("abq.add.full", () -> { ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(1);
            q.add("a"); return q.add("b"); });
        t("abq.offer.full", () -> { ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(1);
            q.add("a"); return q.offer("b"); });
        t("abq.remove.empty", () -> new ArrayBlockingQueue<String>(1).remove());
        t("abq.element.empty", () -> new ArrayBlockingQueue<String>(1).element());
        t("abq.peek.empty", () -> new ArrayBlockingQueue<String>(1).peek());
        t("abq.poll.empty", () -> new ArrayBlockingQueue<String>(1).poll());
        tw("abq.poll.timeout", () -> new ArrayBlockingQueue<String>(1).poll(1, TimeUnit.MILLISECONDS));
        t("abq.poll(null unit)", () -> new ArrayBlockingQueue<String>(1).poll(1, null));
        t("abq.drainTo(self)", () -> { ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(2);
            q.add("a"); return q.drainTo(q); });
        t("abq.drainTo(null)", () -> new ArrayBlockingQueue<String>(2).drainTo(null));
        t("abq.contains(null)", () -> new ArrayBlockingQueue<String>(2).contains(null));
        t("abq.toString", () -> { ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(2);
            q.add("a"); return q.toString(); });
        t("abq.iterator.order", () -> { ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(3);
            q.add("a"); q.add("b"); StringBuilder sb = new StringBuilder();
            for (String s : q) { sb.append(s); } return sb.toString(); });

        // ---- LinkedBlockingQueue -------------------------------------------
        t("lbq.ctor(0)", () -> new LinkedBlockingQueue<String>(0));
        t("lbq.default.remaining", () -> new LinkedBlockingQueue<String>().remainingCapacity());
        t("lbq.offer(null)", () -> new LinkedBlockingQueue<String>().offer(null));
        t("lbq.put(null)", () -> { new LinkedBlockingQueue<String>().put(null); return "no-throw"; });
        t("lbq.ctor(nullColl)", () -> new LinkedBlockingQueue<String>(null));
        t("lbq.ctor(collWithNull)", () -> new LinkedBlockingQueue<>(Arrays.asList("a", null)));
        t("lbq.remove.empty", () -> new LinkedBlockingQueue<String>().remove());
        t("lbq.drainTo(self)", () -> { LinkedBlockingQueue<String> q = new LinkedBlockingQueue<>();
            q.add("a"); return q.drainTo(q); });
        t("lbq.drainTo(max -1)", () -> new LinkedBlockingQueue<String>().drainTo(new ArrayList<>(), -1));

        // ---- LinkedBlockingDeque -------------------------------------------
        t("lbd.ctor(0)", () -> new LinkedBlockingDeque<String>(0));
        t("lbd.offerFirst(null)", () -> new LinkedBlockingDeque<String>().offerFirst(null));
        t("lbd.getFirst.empty", () -> new LinkedBlockingDeque<String>().getFirst());
        t("lbd.pop.empty", () -> new LinkedBlockingDeque<String>().pop());
        t("lbd.order", () -> { LinkedBlockingDeque<String> d = new LinkedBlockingDeque<>();
            d.addFirst("a"); d.addLast("b"); return d.peekFirst() + d.peekLast(); });

        // ---- PriorityBlockingQueue -----------------------------------------
        t("pbq.ctor(0)", () -> new PriorityBlockingQueue<String>(0));
        t("pbq.offer(null)", () -> new PriorityBlockingQueue<String>().offer(null));
        t("pbq.order", () -> { PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
            q.add(3); q.add(1); q.add(2); return q.poll() + "," + q.poll() + "," + q.poll(); });
        t("pbq.remainingCapacity", () -> new PriorityBlockingQueue<String>().remainingCapacity());
        t("pbq.nonComparable", () -> { PriorityBlockingQueue<Object> q = new PriorityBlockingQueue<>();
            q.add(new Object()); q.add(new Object()); return "no-throw"; });

        // ---- SynchronousQueue / LinkedTransferQueue / DelayQueue -----------
        t("sq.offer.noTaker", () -> new SynchronousQueue<String>().offer("a"));
        t("sq.peek", () -> new SynchronousQueue<String>().peek());
        t("sq.isEmpty", () -> new SynchronousQueue<String>().isEmpty());
        t("sq.remainingCapacity", () -> new SynchronousQueue<String>().remainingCapacity());
        t("ltq.offer(null)", () -> new LinkedTransferQueue<String>().offer(null));
        t("ltq.hasWaitingConsumer", () -> new LinkedTransferQueue<String>().hasWaitingConsumer());
        t("ltq.getWaitingConsumerCount", () -> new LinkedTransferQueue<String>().getWaitingConsumerCount());
        t("dq.offer(null)", () -> new DelayQueue<Delayed>().offer(null));
        t("dq.poll.empty", () -> new DelayQueue<Delayed>().poll());

        // ---- ConcurrentLinkedQueue / Deque ---------------------------------
        t("clq.offer(null)", () -> new ConcurrentLinkedQueue<String>().offer(null));
        t("clq.add(null)", () -> new ConcurrentLinkedQueue<String>().add(null));
        t("clq.poll.empty", () -> new ConcurrentLinkedQueue<String>().poll());
        t("clq.remove.empty", () -> new ConcurrentLinkedQueue<String>().remove());
        t("clq.contains(null)", () -> new ConcurrentLinkedQueue<String>().contains(null));
        t("clq.size", () -> { ConcurrentLinkedQueue<String> q = new ConcurrentLinkedQueue<>();
            q.add("a"); q.add("b"); return q.size(); });
        t("clq.toString", () -> { ConcurrentLinkedQueue<String> q = new ConcurrentLinkedQueue<>();
            q.add("a"); return q.toString(); });
        t("cld.offerFirst(null)", () -> new ConcurrentLinkedDeque<String>().offerFirst(null));
        t("cld.getLast.empty", () -> new ConcurrentLinkedDeque<String>().getLast());

        // ---- CopyOnWriteArrayList ------------------------------------------
        t("cowl.add(null)", () -> new CopyOnWriteArrayList<String>().add(null));
        t("cowl.get(0).empty", () -> new CopyOnWriteArrayList<String>().get(0));
        t("cowl.set(0).empty", () -> new CopyOnWriteArrayList<String>().set(0, "a"));
        t("cowl.addIfAbsent", () -> { CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
            l.add("a"); return l.addIfAbsent("a"); });
        t("cowl.iterator.remove", () -> { CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
            l.add("a"); Iterator<String> it = l.iterator(); it.next(); it.remove(); return "no-throw"; });
        t("cowl.ctor(null)", () -> new CopyOnWriteArrayList<String>((Collection<String>) null));
        t("cowl.indexOf(null)", () -> new CopyOnWriteArrayList<String>().indexOf(null));
        t("cowl.subList(0,2)", () -> new CopyOnWriteArrayList<>(new String[] {"a"}).subList(0, 2));
        t("cowl.equals.list", () -> new CopyOnWriteArrayList<>(new String[] {"a"})
                .equals(Arrays.asList("a")));
        t("cows.add(null)", () -> new CopyOnWriteArraySet<String>().add(null));
        t("cows.dedup", () -> { CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
            s.add("a"); s.add("a"); return s.size(); });

        // ---- ConcurrentSkipListMap / Set -----------------------------------
        t("cslm.put(null,v)", () -> new ConcurrentSkipListMap<String, String>().put(null, "v"));
        t("cslm.put(k,null)", () -> new ConcurrentSkipListMap<String, String>().put("k", null));
        t("cslm.get(null)", () -> new ConcurrentSkipListMap<String, String>().get(null));
        t("cslm.containsKey(null)", () -> new ConcurrentSkipListMap<String, String>().containsKey(null));
        t("cslm.firstKey.empty", () -> new ConcurrentSkipListMap<String, String>().firstKey());
        t("cslm.lastEntry.empty", () -> new ConcurrentSkipListMap<String, String>().lastEntry());
        t("cslm.order", () -> { ConcurrentSkipListMap<String, String> m = new ConcurrentSkipListMap<>();
            m.put("b", "1"); m.put("a", "2"); return m.firstKey() + m.lastKey(); });
        t("cslm.headMap(null)", () -> new ConcurrentSkipListMap<String, String>().headMap(null));
        t("cslm.subMap.badRange", () -> new ConcurrentSkipListMap<String, String>().subMap("z", "a"));
        t("cslm.nonComparable", () -> { ConcurrentSkipListMap<Object, String> m = new ConcurrentSkipListMap<>();
            m.put(new Object(), "v"); return "no-throw"; });
        t("cslm.descendingMap", () -> { ConcurrentSkipListMap<String, String> m = new ConcurrentSkipListMap<>();
            m.put("a", "1"); m.put("b", "2"); return m.descendingMap().firstKey(); });
        t("cslm.toString", () -> { ConcurrentSkipListMap<String, String> m = new ConcurrentSkipListMap<>();
            m.put("a", "1"); return m.toString(); });
        t("csls.add(null)", () -> new ConcurrentSkipListSet<String>().add(null));
        t("csls.first.empty", () -> new ConcurrentSkipListSet<String>().first());
        t("csls.order", () -> { ConcurrentSkipListSet<String> s = new ConcurrentSkipListSet<>();
            s.add("b"); s.add("a"); return s.first() + s.last(); });

        System.out.println("SWEEP-DONE");
    }
}
