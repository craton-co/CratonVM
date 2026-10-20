import java.lang.ref.*;
import java.util.*;

/** SBR2 W8 SMALL: java.lang.ref rows (Weak/Soft/Phantom references and ReferenceQueue), cold and after gc. */
public class Sbr2W8SmallRef {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); } }
    static Object newObj() { return new Object() { public String toString() { return "obj"; } }; }
    static void gc() { for (int i = 0; i < 4; i++) { System.gc(); try { Thread.sleep(20); } catch (InterruptedException e) { } } }

    public static void main(String[] a) throws Throwable {
        Object strong = new Object();
        t("weak.get", () -> new WeakReference<>(strong).get() == strong);
        t("weak.refersTo", () -> { var w = new WeakReference<>(strong); return w.refersTo(strong) + "," + w.refersTo(null) + "," + w.refersTo(new Object()); });
        t("weak.clear", () -> { var w = new WeakReference<>(new Object()); w.clear(); return w.get() + "," + w.refersTo(null); });
        t("weak.null referent", () -> { var w = new WeakReference<>(null); return w.get() + "," + w.refersTo(null); });
        t("weak.enqueue", () -> { var q = new ReferenceQueue<Object>(); var w = new WeakReference<>(new Object(), q); boolean e = w.enqueue(); return e + "," + (q.poll() == w) + "," + w.get() + "," + w.enqueue(); });
        t("weak.isEnqueued none", () -> { var q = new ReferenceQueue<Object>(); var w = new WeakReference<>(strong, q); return w.refersTo(strong) + "," + (q.poll() == null); });
        t("weak.gc clears", () -> { var q = new ReferenceQueue<Object>(); var w = new WeakReference<>(newObj(), q); gc(); Reference<?> r = q.remove(2000); return w.get() + "," + (r == w); });
        t("weak.gc keeps strong", () -> { Object o = newObj(); var q = new ReferenceQueue<Object>(); var w = new WeakReference<>(o, q); gc(); return (w.get() == o) + "," + (q.poll() == null) + "," + (o.toString()); });
        t("soft.get", () -> new SoftReference<>(strong).get() == strong);
        t("soft.ctor queue", () -> { var q = new ReferenceQueue<Object>(); var s = new SoftReference<>(strong, q); return s.get() == strong; });
        t("soft.gc keeps", () -> { var s = new SoftReference<>(newObj()); gc(); return s.get() != null; });
        t("soft.clear", () -> { var s = new SoftReference<>(strong); s.clear(); return s.get(); });
        t("soft.enqueue", () -> { var q = new ReferenceQueue<Object>(); var s = new SoftReference<>(strong, q); return s.enqueue() + "," + (q.poll() == s) + "," + s.get(); });
        t("soft.refersTo", () -> new SoftReference<>(strong).refersTo(strong));
        t("phantom.get", () -> { var q = new ReferenceQueue<Object>(); var p = new PhantomReference<>(strong, q); return p.get() + "," + p.refersTo(strong); });
        t("phantom.ctor null queue", () -> new PhantomReference<>(strong, null).get());
        t("phantom.gc enqueues", () -> { var q = new ReferenceQueue<Object>(); var p = new PhantomReference<>(newObj(), q); gc(); Reference<?> r = q.remove(2000); return (r == p) + "," + p.get(); });
        t("phantom.clear", () -> { var q = new ReferenceQueue<Object>(); var p = new PhantomReference<>(newObj(), q); p.clear(); gc(); return q.poll() + "," + p.refersTo(null); });
        t("queue.poll empty", () -> new ReferenceQueue<Object>().poll());
        t("queue.remove timeout", () -> { long t0 = System.nanoTime(); var r = new ReferenceQueue<Object>().remove(50); long ms = (System.nanoTime() - t0) / 1000000; return r + "," + (ms >= 45 && ms < 2000); });
        t("queue.remove neg", () -> new ReferenceQueue<Object>().remove(-1));
        t("queue.remove interrupted", () -> { Thread.currentThread().interrupt(); try { return new ReferenceQueue<Object>().remove(1000); } finally { Thread.interrupted(); } });
        t("queue.fifo", () -> { var q = new ReferenceQueue<Object>(); var w1 = new WeakReference<>(strong, q); var w2 = new WeakReference<>(strong, q); var w3 = new WeakReference<>(strong, q); w1.enqueue(); w2.enqueue(); w3.enqueue(); return (q.poll() == w3) + "," + (q.poll() == w2) + "," + (q.poll() == w1) + "," + q.poll(); });
        t("queue.remove blocks then wakes", () -> { var q = new ReferenceQueue<Object>(); var w = new WeakReference<>(strong, q); var res = new Object[1]; var th = new Thread(() -> { try { res[0] = q.remove(); } catch (InterruptedException e) { res[0] = "intr"; } }); th.start(); Thread.sleep(50); w.enqueue(); th.join(3000); return res[0] == w; });
        t("fence", () -> { Reference.reachabilityFence(strong); Reference.reachabilityFence(null); return "ok"; });
        t("subclass", () -> { class R extends WeakReference<Object> { R(Object o) { super(o); } } var r = new R(strong); return r.get() == strong && r instanceof Reference; });
        t("many weak gc", () -> { var q = new ReferenceQueue<Object>(); var keep = new ArrayList<WeakReference<Object>>(); for (int i = 0; i < 500; i++) keep.add(new WeakReference<>(new byte[128], q)); gc(); int n = 0; Reference<?> r; while ((r = q.poll()) != null) n++; int live = 0; for (var w : keep) if (w.get() != null) live++; return (n + live) + "," + (live == 0); });
        t("soft.pressure", () -> {
            // a soft referent is cleared before OutOfMemoryError (HotSpot); run with a small heap (-Xmx256m)
            var s = new SoftReference<>(new byte[24 << 20]);
            var hold = new ArrayList<byte[]>();
            boolean oom = false;
            try { for (int i = 0; i < 400 && s.get() != null; i++) hold.add(new byte[8 << 20]); } catch (OutOfMemoryError e) { oom = true; }
            boolean cleared = s.get() == null;
            hold.clear();
            return "cleared=" + cleared + " oomBeforeClear=" + (oom && !cleared);
        });
        t("hot soft.get", () -> { var s = new SoftReference<>(strong); long n = 0; for (int i = 0; i < 300000; i++) if (s.get() == strong) n++; return n; });
        t("hot queue.poll", () -> { var q = new ReferenceQueue<Object>(); long n = 0; for (int i = 0; i < 300000; i++) if (q.poll() == null) n++; return n; });
        t("hot queue enqueue/poll", () -> { var q = new ReferenceQueue<Object>(); long n = 0; for (int i = 0; i < 20000; i++) { var w = new WeakReference<>(strong, q); w.enqueue(); if (q.poll() == w) n++; } return n; });
        t("hot clear", () -> { long n = 0; for (int i = 0; i < 100000; i++) { var s = new SoftReference<>(strong); s.clear(); if (s.get() == null) n++; } return n; });
        t("weakmap", () -> { var m = new WeakHashMap<Object, String>(); Object k = new Object(); m.put(k, "v"); m.put(newObj(), "w"); gc(); return m.size() + "," + m.get(k); });
        t("cleaner", () -> { var c = java.lang.ref.Cleaner.create(); var fired = new boolean[1]; Object o = newObj(); c.register(o, () -> fired[0] = true); o = null; gc(); Thread.sleep(200); return fired[0]; });
    }
}
