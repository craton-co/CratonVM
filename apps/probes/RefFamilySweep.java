import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;

/** L8 tail — `java.lang.ref`: 19 rows across `Reference`, `ReferenceQueue`,
 *  `SoftReference`, `WeakReference` and `PhantomReference`.
 *
 *  THE HARD PART IS NOT THE API, IT IS THE ORACLE. Almost every interesting
 *  question about a reference — is it cleared yet, is it enqueued yet — is
 *  answered by the collector, and two collectors are allowed to answer
 *  differently at any given moment. A probe that called `System.gc()` and then
 *  asked `get()` would be measuring GC policy, and it would be right about a
 *  different thing each run.
 *
 *  So every row here is one the SPECIFICATION fixes regardless of collector
 *  state, and the referent is STRONGLY HELD throughout so the collector has no
 *  licence to act:
 *
 *    * `get()` on a strongly-reachable referent is that referent;
 *    * `PhantomReference.get()` is null even then, and `refersTo` still says
 *      yes — the pair that separates the two accessors;
 *    * `clear()` is caller-driven and immediate, so `get()`/`refersTo` after it
 *      are fixed;
 *    * `enqueue()` is caller-driven too: true once, false thereafter, and false
 *      forever for a reference with no queue;
 *    * a queue polled when nothing was enqueued is empty;
 *    * `remove(timeout)` with a negative timeout is a refusal, not a wait.
 *
 *  `ReferenceQueue.remove()` with no timeout blocks until something arrives, so
 *  it is only ever called here AFTER an explicit `enqueue()`.
 */
public class RefFamilySweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    /** A referent with a stable rendering — `Object.toString()` carries an
     *  identity hash and would differ on every run and every VM. */
    static final class Referent {
        final String name;

        Referent(String name) {
            this.name = name;
        }

        @Override
        public String toString() {
            return "Referent(" + name + ")";
        }
    }

    /** Held in a static so nothing here is collectible while the probe runs. */
    static final Referent A = new Referent("a");
    static final Referent B = new Referent("b");

    // ------------------------------------------------------ 1. construction

    static void construction() {
        p("WeakReference(a).get", () -> new WeakReference<>(A).get());
        p("WeakReference(a,null queue).get", () -> new WeakReference<>(A, null).get());
        p("WeakReference(null).get", () -> new WeakReference<>(null).get());
        p("WeakReference(null,null).get", () -> new WeakReference<>(null, null).get());
        p("SoftReference(a).get", () -> new SoftReference<>(A).get());
        p("SoftReference(a,null queue).get", () -> new SoftReference<>(A, null).get());
        p("SoftReference(null).get", () -> new SoftReference<>(null).get());
        p("PhantomReference(a,null queue).get", () -> new PhantomReference<>(A, null).get());
        p("PhantomReference(null,null).get", () -> new PhantomReference<>(null, null).get());
        p("WeakReference with a queue.get", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            return new WeakReference<>(A, q).get();
        });
        p("SoftReference with a queue.get", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            return new SoftReference<>(A, q).get();
        });
        p("PhantomReference with a queue.get", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            return new PhantomReference<>(A, q).get();
        });
        p("WeakReference class", () -> new WeakReference<>(A).getClass().getName());
        p("SoftReference class", () -> new SoftReference<>(A).getClass().getName());
        p("PhantomReference class", () -> new PhantomReference<>(A, null).getClass().getName());
        p("WeakReference is a Reference", () -> new WeakReference<>(A) instanceof Reference);
        p("SoftReference is a Reference", () -> new SoftReference<>(A) instanceof Reference);
        p("PhantomReference is a Reference",
            () -> new PhantomReference<>(A, null) instanceof Reference);
    }

    // ---------------------------------------------------- 2. get vs refersTo

    /** The pair a phantom reference exists to separate: `get()` is always null
     *  there, and `refersTo` still answers. A VM that implements one in terms
     *  of the other gets exactly these rows wrong. */
    static void accessors() {
        p("weak refersTo its referent", () -> new WeakReference<>(A).refersTo(A));
        p("weak refersTo another", () -> new WeakReference<>(A).refersTo(B));
        p("weak refersTo null", () -> new WeakReference<>(A).refersTo(null));
        p("weak on null refersTo null", () -> new WeakReference<>(null).refersTo(null));
        p("weak on null refersTo a", () -> new WeakReference<Referent>(null).refersTo(A));
        p("soft refersTo its referent", () -> new SoftReference<>(A).refersTo(A));
        p("soft refersTo another", () -> new SoftReference<>(A).refersTo(B));
        p("phantom get is null", () -> new PhantomReference<>(A, null).get());
        p("phantom refersTo its referent", () -> new PhantomReference<>(A, null).refersTo(A));
        p("phantom refersTo another", () -> new PhantomReference<>(A, null).refersTo(B));
        p("phantom refersTo null", () -> new PhantomReference<>(A, null).refersTo(null));
        p("get twice is the same object", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            return r.get() == r.get();
        });
        p("get is the referent by identity", () -> new WeakReference<>(A).get() == A);
    }

    // --------------------------------------------------------- 3. clear

    static void clearing() {
        p("clear then get", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            r.clear();
            return r.get();
        });
        p("clear then refersTo the referent", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            r.clear();
            return r.refersTo(A);
        });
        p("clear then refersTo null", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            r.clear();
            return r.refersTo(null);
        });
        p("clear twice", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            r.clear();
            r.clear();
            return r.get();
        });
        p("clear a soft reference", () -> {
            SoftReference<Referent> r = new SoftReference<>(A);
            r.clear();
            return r.get();
        });
        p("clear a phantom reference", () -> {
            PhantomReference<Referent> r = new PhantomReference<>(A, null);
            r.clear();
            return r.refersTo(A);
        });
        p("clear does not affect a sibling reference", () -> {
            WeakReference<Referent> r1 = new WeakReference<>(A);
            WeakReference<Referent> r2 = new WeakReference<>(A);
            r1.clear();
            return r1.get() + "/" + r2.get();
        });
        p("clear does not affect the referent", () -> {
            WeakReference<Referent> r = new WeakReference<>(A);
            r.clear();
            return A.toString();
        });
    }

    // -------------------------------------------------------- 4. enqueue

    static void enqueueing() {
        p("enqueue with no queue", () -> new WeakReference<>(A).enqueue());
        p("enqueue with a queue", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            return new WeakReference<>(A, q).enqueue();
        });
        p("enqueue twice", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            return r.enqueue() + "/" + r.enqueue();
        });
        p("enqueue then poll", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            r.enqueue();
            return q.poll() == r;
        });
        p("enqueue then poll twice", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            r.enqueue();
            q.poll();
            return String.valueOf(q.poll());
        });
        p("enqueue then remove", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            r.enqueue();
            return q.remove() == r;
        });
        p("enqueue clears the referent", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            r.enqueue();
            return String.valueOf(r.get());
        });
        p("enqueue order is preserved", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r1 = new WeakReference<>(A, q);
            WeakReference<Referent> r2 = new WeakReference<>(B, q);
            r1.enqueue();
            r2.enqueue();
            Object a = q.poll();
            Object b = q.poll();
            return (a == r1) + "/" + (b == r2);
        });
        p("enqueue after clear", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            WeakReference<Referent> r = new WeakReference<>(A, q);
            r.clear();
            return r.enqueue();
        });
        p("phantom enqueue", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            PhantomReference<Referent> r = new PhantomReference<>(A, q);
            return r.enqueue() + "/" + (q.poll() == r);
        });
        p("soft enqueue", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            SoftReference<Referent> r = new SoftReference<>(A, q);
            return r.enqueue() + "/" + (q.poll() == r);
        });
    }

    // ---------------------------------------------------------- 5. the queue

    static void queues() {
        p("poll an empty queue", () -> String.valueOf(new ReferenceQueue<>().poll()));
        p("poll twice on an empty queue", () -> {
            ReferenceQueue<Referent> q = new ReferenceQueue<>();
            q.poll();
            return String.valueOf(q.poll());
        });
        p("remove with a timeout on an empty queue",
            () -> String.valueOf(new ReferenceQueue<>().remove(20)));
        p("remove(0) is not asked", () -> "blocks forever by specification");
        p("remove with a negative timeout", () -> {
            try {
                return String.valueOf(new ReferenceQueue<>().remove(-1));
            } catch (Throwable e) {
                return "THREW " + e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("queue class", () -> new ReferenceQueue<>().getClass().getName());
        p("a reference enqueued to one queue is not in another", () -> {
            ReferenceQueue<Referent> q1 = new ReferenceQueue<>();
            ReferenceQueue<Referent> q2 = new ReferenceQueue<>();
            new WeakReference<>(A, q1).enqueue();
            return String.valueOf(q2.poll());
        });
    }

    // ------------------------------------------------- 6. reachabilityFence

    static void fences() {
        p("reachabilityFence of an object", () -> {
            Reference.reachabilityFence(A);
            return "no throw";
        });
        p("reachabilityFence of null", () -> {
            Reference.reachabilityFence(null);
            return "no throw";
        });
        p("reachabilityFence returns void and keeps the object usable", () -> {
            Referent r = new Referent("fenced");
            String s = r.toString();
            Reference.reachabilityFence(r);
            return s;
        });
    }

    public static void main(String[] args) {
        sect("construction", RefFamilySweep::construction);
        sect("accessors", RefFamilySweep::accessors);
        sect("clearing", RefFamilySweep::clearing);
        sect("enqueueing", RefFamilySweep::enqueueing);
        sect("queues", RefFamilySweep::queues);
        sect("fences", RefFamilySweep::fences);
        System.out.println("rows " + rows);
        System.out.println("DONE RefFamilySweep");
    }
}
