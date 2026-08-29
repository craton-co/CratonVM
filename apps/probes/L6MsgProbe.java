import java.util.*;
import java.util.concurrent.*;

/** The MESSAGE of every exception this lane is about to start throwing.
 *
 *  A native that throws the right TYPE with an invented message is a defect
 *  this campaign has recorded ten times in this family alone (`G1-1`, "the CHM
 *  message that was invented"). `null` below means the JDK used the no-arg
 *  constructor and the native must too; anything else is the exact string.
 */
public class L6MsgProbe {
    static void m(String tag, Run r) {
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) {
            System.out.println(tag + " |" + e.getClass().getName() + "| msg |"
                               + e.getMessage() + "|");
        }
    }
    interface Run { void run() throws Throwable; }

    public static void main(String[] a) {
        m("chm(-1)", () -> new ConcurrentHashMap<>(-1));
        m("chm(16,0f)", () -> new ConcurrentHashMap<>(16, 0f));
        m("chm(16,NaN)", () -> new ConcurrentHashMap<>(16, Float.NaN));
        m("chm(16,.75,0)", () -> new ConcurrentHashMap<>(16, 0.75f, 0));
        m("chm(null map)", () -> new ConcurrentHashMap<Object, Object>(null));
        m("chm putAll(null)", () -> new ConcurrentHashMap<>().putAll(null));
        m("chm containsValue(null)", () -> new ConcurrentHashMap<>().containsValue(null));
        m("chm contains(null)", () -> new ConcurrentHashMap<>().contains(null));
        m("chm values contains(null)", () -> new ConcurrentHashMap<>().values().contains(null));
        m("chm newKeySet(-1)", () -> ConcurrentHashMap.newKeySet(-1));
        m("chm replaceAll -> null", () -> {
            ConcurrentHashMap<String, Integer> x = new ConcurrentHashMap<>();
            x.put("a", 1);
            x.replaceAll((k, v) -> null);
        });
        m("chm recursive computeIfAbsent", () -> {
            ConcurrentHashMap<String, Integer> x = new ConcurrentHashMap<>();
            x.computeIfAbsent("q", k -> x.computeIfAbsent("q", k2 -> 1));
        });
        m("chm recursive compute", () -> {
            ConcurrentHashMap<String, Integer> x = new ConcurrentHashMap<>();
            x.compute("q", (k, v) -> x.compute("q", (k2, v2) -> 1));
        });
        m("chm elements past end", () -> new ConcurrentHashMap<>().elements().nextElement());

        m("Thread(String null)", () -> new Thread((String) null));
        m("Thread(Runnable, null)", () -> new Thread(() -> { }, (String) null));
        m("Thread(group, r, null, 0)", () -> new Thread(null, () -> { }, null, 0L));
        m("setDaemon while alive", () -> {
            Thread t = new Thread(() -> { try { Thread.sleep(300); } catch (Exception e) { } });
            t.start();
            try { t.setDaemon(true); } finally { t.interrupt(); t.join(); }
        });
        m("virtual setDaemon(false)",
          () -> Thread.ofVirtual().unstarted(() -> { }).setDaemon(false));

        m("fjt cancelled join", () -> {
            ForkJoinTask<?> t = ForkJoinTask.adapt(() -> { });
            t.cancel(false);
            t.join();
        });
        m("fjt cancelled get", () -> {
            ForkJoinTask<?> t = ForkJoinTask.adapt(() -> { });
            t.cancel(false);
            t.get();
        });
        m("fjt cancelled invoke", () -> {
            ForkJoinTask<?> t = ForkJoinTask.adapt(() -> { });
            t.cancel(false);
            t.invoke();
        });
        ForkJoinPool p = new ForkJoinPool(2);
        m("fjp submit(null Callable)", () -> p.submit((Callable<Integer>) null));
        m("fjp submit(null Runnable)", () -> p.submit((Runnable) null));
        m("fjp submit(null task)", () -> p.submit((ForkJoinTask<Integer>) null));
        m("fjp execute(null Runnable)", () -> p.execute((Runnable) null));
        m("fjp execute(null task)", () -> p.execute((ForkJoinTask<?>) null));
        m("fjp invoke(null)", () -> p.invoke(null));
        m("fjp invoke thrower", () -> p.invoke(new RecursiveTask<Integer>() {
            protected Integer compute() { throw new IllegalStateException("l6-boom"); }
        }));
        p.shutdown();
        m("fjp submit after shutdown", () -> p.submit(() -> 1));
        m("fjp execute after shutdown", () -> p.execute(() -> { }));
        m("fjp invoke after shutdown", () -> p.invoke(ForkJoinTask.adapt(() -> { })));
        System.out.println("DONE L6MsgProbe");
    }
}
