import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.locks.*;

/**
 * SBR round 2, wave 7, lane THREAD: real thread state. Every row is a value
 * HotSpot 25 answers deterministically; the probe runs the rows once, then 20
 * more times, and prints DRIFT if a hot re-run differs from the first.
 *
 *   java Sbr2W7Thread.java            (HotSpot)
 *   cratonvm --jdk-only ... Sbr2W7Thread.java / compiled class
 */
public class Sbr2W7Thread {
    static final List<String> out = new ArrayList<>();

    static void row(String k, Object v) {
        out.add(k + " = " + v);
    }

    static Thread.State waitState(Thread t, Thread.State s, long ms) {
        long end = System.nanoTime() + ms * 1_000_000L;
        Thread.State cur = t.getState();
        while (cur != s && System.nanoTime() < end) {
            Thread.onSpinWait();
            try {
                Thread.sleep(1);
            } catch (InterruptedException e) {
                return null;
            }
            cur = t.getState();
        }
        return cur;
    }

    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>();
    static final InheritableThreadLocal<String> ITL_CV = new InheritableThreadLocal<>() {
        @Override
        protected String childValue(String p) {
            return p == null ? null : p + "+child";
        }
    };
    static final ThreadLocal<Integer> TL = ThreadLocal.withInitial(() -> 7);
    static final ThreadLocal<String> TL_INIT = new ThreadLocal<>() {
        @Override
        protected String initialValue() {
            return "init:" + Thread.currentThread().getName();
        }
    };

    static String shape(String n) {
        return n == null ? null : n.replaceAll("\\d+", "#");
    }

    static void states() throws Exception {
        Thread t = new Thread(() -> {});
        row("state.new", t.getState());
        row("alive.new", t.isAlive());
        row("state.main", Thread.currentThread().getState());
        row("alive.main", Thread.currentThread().isAlive());

        // RUNNABLE (spinning), then TERMINATED
        volatileStop = false;
        Thread spin = new Thread(() -> {
            while (!volatileStop) {
                Thread.onSpinWait();
            }
        }, "spin");
        spin.start();
        row("alive.started", spin.isAlive());
        row("state.spin", waitState(spin, Thread.State.RUNNABLE, 2000));
        volatileStop = true;
        spin.join();
        row("alive.joined", spin.isAlive());
        row("state.joined", spin.getState());

        // BLOCKED on a monitor main holds
        Object mon = new Object();
        Thread blocked = new Thread(() -> {
            synchronized (mon) {
            }
        }, "blocked");
        synchronized (mon) {
            blocked.start();
            row("state.blocked", waitState(blocked, Thread.State.BLOCKED, 3000));
        }
        blocked.join();
        row("state.blocked.after", blocked.getState());

        // WAITING in Object.wait
        Object wm = new Object();
        Thread waiting = new Thread(() -> {
            synchronized (wm) {
                try {
                    wm.wait();
                } catch (InterruptedException e) {
                }
            }
        }, "waiting");
        waiting.start();
        row("state.wait", waitState(waiting, Thread.State.WAITING, 3000));
        synchronized (wm) {
            wm.notifyAll();
        }
        waiting.join();

        // TIMED_WAITING in Object.wait(ms)
        Object tm = new Object();
        Thread timed = new Thread(() -> {
            synchronized (tm) {
                try {
                    tm.wait(30_000);
                } catch (InterruptedException e) {
                }
            }
        }, "timed");
        timed.start();
        row("state.wait.timed", waitState(timed, Thread.State.TIMED_WAITING, 3000));
        timed.interrupt();
        timed.join();

        // TIMED_WAITING in sleep
        Thread sleeper = new Thread(() -> {
            try {
                Thread.sleep(30_000);
            } catch (InterruptedException e) {
                row0("sleeper.interrupted", Thread.currentThread().isInterrupted());
            }
        }, "sleeper");
        sleeper.start();
        row("state.sleep", waitState(sleeper, Thread.State.TIMED_WAITING, 3000));
        sleeper.interrupt();
        sleeper.join();
        row("state.sleep.after", sleeper.getState());

        // WAITING in LockSupport.park / TIMED_WAITING in parkNanos
        Thread parked = new Thread(() -> LockSupport.park(), "parked");
        parked.start();
        row("state.park", waitState(parked, Thread.State.WAITING, 3000));
        LockSupport.unpark(parked);
        parked.join();
        Thread parkedT = new Thread(() -> LockSupport.parkNanos(30_000_000_000L), "parkedT");
        parkedT.start();
        row("state.parkNanos", waitState(parkedT, Thread.State.TIMED_WAITING, 3000));
        LockSupport.unpark(parkedT);
        parkedT.join();

        // WAITING in join, in a lock condition, in a queue
        Thread target = new Thread(() -> {
            try {
                Thread.sleep(300);
            } catch (InterruptedException e) {
            }
        }, "joinTarget");
        Thread joiner = new Thread(() -> {
            try {
                target.join();
            } catch (InterruptedException e) {
            }
        }, "joiner");
        target.start();
        joiner.start();
        row("state.join", waitState(joiner, Thread.State.WAITING, 3000));
        joiner.join();
        target.join();
        row("state.join.after", joiner.getState() + "/" + target.getState());

        ReentrantLock lock = new ReentrantLock();
        Condition c = lock.newCondition();
        Thread cw = new Thread(() -> {
            lock.lock();
            try {
                c.await();
            } catch (InterruptedException e) {
            } finally {
                lock.unlock();
            }
        }, "condWaiter");
        cw.start();
        row("state.condition", waitState(cw, Thread.State.WAITING, 3000));
        cw.interrupt();
        cw.join();

        BlockingQueue<Integer> q = new ArrayBlockingQueue<>(1);
        Thread taker = new Thread(() -> {
            try {
                q.take();
            } catch (InterruptedException e) {
            }
        }, "taker");
        taker.start();
        row("state.queue.take", waitState(taker, Thread.State.WAITING, 3000));
        q.put(1);
        taker.join();

        // start twice
        Thread once = new Thread(() -> {});
        once.start();
        once.join();
        try {
            once.start();
            row("start.twice", "no exception");
        } catch (IllegalThreadStateException e) {
            row("start.twice", "ITSE " + e.getMessage());
        }
        Thread never = new Thread(() -> {});
        row("join.unstarted", tryJoin(never));
        row("state.after.terminate.isAlive", once.isAlive());
    }

    static volatile boolean volatileStop;

    static String tryJoin(Thread t) {
        try {
            t.join(50);
            return "ok alive=" + t.isAlive() + " " + t.getState();
        } catch (InterruptedException e) {
            return "interrupted";
        }
    }

    // the sleeper writes to `out` from another thread: keep it out of the compared rows
    static void row0(String k, Object v) {
    }

    static void locals() throws Exception {
        TL.set(11);
        row("tl.main", TL.get());
        final Object[] seen = new Object[6];
        Thread t = new Thread(() -> {
            seen[0] = TL.get();
            TL.set(99);
            seen[1] = TL.get();
            seen[2] = TL_INIT.get().startsWith("init:") ? "init" : "?";
            TL.remove();
            seen[3] = TL.get();
        });
        t.start();
        t.join();
        row("tl.child", Arrays.toString(seen));
        row("tl.main.after", TL.get());
        TL_INIT.set("mine");
        row("tl.init.main", TL_INIT.get());
        TL.remove();
        row("tl.removed", TL.get());

        // InheritableThreadLocal: construction-time capture
        ITL.set("parent");
        ITL_CV.set("p");
        String[] r = new String[8];
        Thread child = new Thread(() -> {
            r[0] = ITL.get();
            r[1] = ITL_CV.get();
        });
        ITL.set("later");
        ITL_CV.set("p2");
        child.start();
        child.join();
        row("itl.inherit", r[0] + " " + r[1]);

        // inheritThreadLocals = false
        Thread noInh = new Thread(null, () -> {
            r[2] = ITL.get();
        }, "noInherit", 0, false);
        noInh.start();
        noInh.join();
        row("itl.noinherit", r[2]);
        Thread inh = new Thread(null, () -> {
            r[3] = ITL.get();
        }, "inherit", 0, true);
        inh.start();
        inh.join();
        row("itl.inherit.true", r[3]);

        // grandchild (transitive) without the child reading it
        Thread mid = new Thread(() -> {
            Thread g = new Thread(() -> {
                r[4] = ITL.get() + " " + ITL_CV.get();
            });
            g.start();
            try {
                g.join();
            } catch (InterruptedException e) {
            }
        });
        ITL.set("gp");
        ITL_CV.set("g");
        mid.start();
        mid.join();
        row("itl.grandchild", r[4]);

        // remove before construction: child sees null
        ITL.remove();
        ITL_CV.remove();
        Thread none = new Thread(() -> {
            r[5] = ITL.get() + " " + ITL_CV.get();
        });
        ITL.set("after");
        none.start();
        none.join();
        row("itl.removed.before.ctor", r[5]);

        // executor threads: ThreadLocal per worker
        ITL.set("pool-parent");
        ExecutorService ex = Executors.newFixedThreadPool(2);
        Future<String> f = ex.submit(() -> ITL.get());
        row("itl.executor", f.get());
        ex.shutdown();
        ex.awaitTermination(5, TimeUnit.SECONDS);

        // ThreadLocal from a virtual thread
        Thread v = Thread.ofVirtual().unstarted(() -> {
            r[6] = ITL.get() + " " + TL.get();
        });
        v.start();
        v.join();
        row("itl.virtual", r[6]);
        Thread vni = Thread.ofVirtual().inheritInheritableThreadLocals(false).unstarted(() -> {
            r[7] = ITL.get() + "";
        });
        vni.start();
        vni.join();
        row("itl.virtual.noinherit", r[7]);
        ITL.remove();
    }

    static void virtuals() throws Exception {
        CountDownLatch go = new CountDownLatch(1);
        Thread v = Thread.ofVirtual().name("vt-probe").unstarted(() -> {
            try {
                go.await();
            } catch (InterruptedException e) {
            }
        });
        row("v.state.new", v.getState());
        row("v.alive.new", v.isAlive());
        row("v.isVirtual", v.isVirtual());
        row("v.daemon", v.isDaemon());
        row("v.name", v.getName());
        row("v.group", v.getThreadGroup() == null ? null : v.getThreadGroup().getName());
        v.start();
        row("v.alive.started", v.isAlive());
        row("v.state.wait", waitState(v, Thread.State.WAITING, 3000));
        go.countDown();
        v.join();
        row("v.alive.joined", v.isAlive());
        row("v.state.joined", v.getState());
        // join with timeout on a sleeping virtual thread
        Thread vs = Thread.ofVirtual().unstarted(() -> {
            try {
                Thread.sleep(200);
            } catch (InterruptedException e) {
            }
        });
        vs.start();
        row("v.join.timed", vs.join(java.time.Duration.ofMillis(10)));
        row("v.state.sleep", waitState(vs, Thread.State.TIMED_WAITING, 3000));
        vs.join();
        row("v.join.after", vs.getState());
        // startVirtualThread + many joins
        List<Thread> ts = new ArrayList<>();
        int[] cnt = new int[1];
        for (int i = 0; i < 20; i++) {
            ts.add(Thread.startVirtualThread(() -> {
                synchronized (cnt) {
                    cnt[0]++;
                }
            }));
        }
        for (Thread t : ts) {
            t.join();
        }
        row("v.many", cnt[0]);
        StringBuilder sb = new StringBuilder();
        for (Thread t : ts) {
            sb.append(t.isAlive() ? "A" : "d").append(t.getState() == Thread.State.TERMINATED ? "T" : "?");
        }
        row("v.many.states", sb.length() == 40 && sb.toString().replace("dT", "").isEmpty());
        try {
            v.start();
            row("v.start.twice", "no exception");
        } catch (IllegalThreadStateException e) {
            row("v.start.twice", "ITSE " + e.getMessage());
        }
    }

    static void construction() throws Exception {
        Thread a = new Thread();
        Thread b = new Thread();
        row("ctor.default.shape", shape(a.getName()) + " " + shape(b.getName()));
        int na = Integer.parseInt(a.getName().substring("Thread-".length()));
        int nb = Integer.parseInt(b.getName().substring("Thread-".length()));
        row("ctor.default.consecutive", nb - na);
        row("ctor.tid.distinct", a.threadId() != b.threadId() && a.threadId() > 1 && b.threadId() > 1);
        row("ctor.tid.increasing", b.threadId() > a.threadId());
        row("ctor.name", new Thread("named").getName());
        row("ctor.runnable.name", shape(new Thread(() -> {}).getName()));
        row("ctor.runnable.string", new Thread(() -> {}, "rs").getName());
        try {
            new Thread((String) null);
            row("ctor.null.name", "no exception");
        } catch (NullPointerException e) {
            row("ctor.null.name", "NPE " + e.getMessage());
        }
        try {
            new Thread(null, () -> {}, null);
            row("ctor.null.name3", "no exception");
        } catch (NullPointerException e) {
            row("ctor.null.name3", "NPE " + e.getMessage());
        }
        row("ctor.priority", a.getPriority());
        row("ctor.daemon", a.isDaemon());
        row("ctor.group", a.getThreadGroup().getName());
        a.setDaemon(true);
        a.setPriority(3);
        row("ctor.set", a.isDaemon() + " " + a.getPriority());
        ThreadGroup g = new ThreadGroup("probeGroup");
        g.setMaxPriority(4);
        Thread gt = new Thread(g, () -> {}, "ingroup");
        row("ctor.group.custom", gt.getThreadGroup().getName() + " " + gt.getPriority());
        Thread gt2 = new Thread(g, "ingroup2");
        row("ctor.group.name", gt2.getThreadGroup().getName() + " " + gt2.getName());
        Thread gt3 = new Thread(g, () -> {});
        row("ctor.group.runnable", gt3.getThreadGroup().getName() + " " + shape(gt3.getName()));
        Thread gt4 = new Thread(g, () -> {}, "stack", 1 << 20);
        row("ctor.stack", gt4.getName() + " " + gt4.getThreadGroup().getName());
        Thread gt5 = new Thread((ThreadGroup) null, () -> {}, "nullgroup");
        row("ctor.group.null", gt5.getThreadGroup().getName());
        // daemon inherits from the creating thread
        boolean[] d = new boolean[3];
        Thread dparent = new Thread(() -> {
            Thread c = new Thread(() -> {});
            d[0] = c.isDaemon();
        });
        dparent.setDaemon(true);
        dparent.start();
        dparent.join();
        row("ctor.daemon.inherit", d[0]);
        // priority inherits
        int[] p = new int[1];
        Thread pp = new Thread(() -> {
            p[0] = new Thread(() -> {}).getPriority();
        });
        pp.setPriority(7);
        pp.start();
        pp.join();
        row("ctor.priority.inherit", p[0]);
        // context class loader inherits / system loader for noInherit
        ClassLoader cl = new java.net.URLClassLoader(new java.net.URL[0]);
        Thread.currentThread().setContextClassLoader(cl);
        boolean[] ccl = new boolean[2];
        Thread c1 = new Thread(() -> {});
        row("ctor.ccl.inherit", c1.getContextClassLoader() == cl);
        Thread c2 = new Thread(null, () -> {}, "noinh", 0, false);
        row("ctor.ccl.noinherit", c2.getContextClassLoader() == ClassLoader.getSystemClassLoader());
        Thread.currentThread().setContextClassLoader(ClassLoader.getSystemClassLoader());
        // subclass with run override and super(name)
        class Sub extends Thread {
            int ran;

            Sub() {
                super("sub");
            }

            @Override
            public void run() {
                ran++;
            }
        }
        Sub s = new Sub();
        s.start();
        s.join();
        row("ctor.subclass", s.getName() + " " + s.ran + " " + s.getState());
        // thread of a thread
        row("ctor.toString", shape(new Thread("x").toString()));
        // interrupt before start
        Thread ib = new Thread(() -> {});
        ib.interrupt();
        row("interrupt.unstarted", ib.isInterrupted());
    }

    static void handlers() throws Exception {
        Object[] seen = new Object[4];
        Thread t = new Thread(() -> {
            throw new IllegalStateException("boom");
        }, "thrower");
        t.setUncaughtExceptionHandler((th, ex) -> {
            seen[0] = th.getName() + " " + ex.getMessage() + " " + th.getState() + " " + th.isAlive();
        });
        t.start();
        t.join();
        row("uncaught.per-thread", seen[0]);
        row("uncaught.after", t.getUncaughtExceptionHandler() == null ? "null" : "group " + (t.getUncaughtExceptionHandler() == t.getThreadGroup()));
        Thread.UncaughtExceptionHandler prev = Thread.getDefaultUncaughtExceptionHandler();
        Thread.setDefaultUncaughtExceptionHandler((th, ex) -> {
            seen[1] = "default " + th.getName() + " " + ex.getClass().getSimpleName();
        });
        Thread t2 = new Thread(() -> {
            throw new RuntimeException("x");
        }, "thrower2");
        t2.start();
        t2.join();
        row("uncaught.default", seen[1]);
        Thread.setDefaultUncaughtExceptionHandler(prev);
        row("uncaught.handler.unstarted", new Thread(() -> {}).getUncaughtExceptionHandler() != null);
        Thread.UncaughtExceptionHandler h = (th, ex) -> {};
        Thread t3 = new Thread(() -> {});
        t3.setUncaughtExceptionHandler(h);
        row("uncaught.handler.set", t3.getUncaughtExceptionHandler() == h);
        // a thread that finishes normally: getState in a finally-like observer
        Thread t4 = new Thread(() -> {});
        t4.start();
        t4.join();
        row("terminated.handler.group", t4.getUncaughtExceptionHandler() == t4.getThreadGroup() ? "group" : String.valueOf(t4.getUncaughtExceptionHandler()));
        row("terminated.group", t4.getThreadGroup());
    }

    static void misc() throws Exception {
        // join with timeout, interrupted join
        Thread sl = new Thread(() -> {
            try {
                Thread.sleep(400);
            } catch (InterruptedException e) {
            }
        }, "sl");
        sl.start();
        long t0 = System.nanoTime();
        sl.join(20);
        row("join.timed.alive", sl.isAlive());
        Thread main = Thread.currentThread();
        Thread irq = new Thread(() -> {
            try {
                Thread.sleep(30);
            } catch (InterruptedException e) {
            }
            main.interrupt();
        });
        irq.start();
        try {
            sl.join();
            row("join.interrupted", "returned");
        } catch (InterruptedException e) {
            row("join.interrupted", "IE " + Thread.currentThread().isInterrupted());
        }
        sl.join();
        irq.join();
        // isAlive from within the running thread
        boolean[] self = new boolean[1];
        Thread me = new Thread(() -> {
            self[0] = Thread.currentThread().isAlive();
        });
        me.start();
        me.join();
        row("alive.self", self[0]);
        // state of a thread from inside itself
        Thread[] holder = new Thread[1];
        Thread.State[] st = new Thread.State[1];
        holder[0] = new Thread(() -> st[0] = holder[0].getState());
        holder[0].start();
        holder[0].join();
        row("state.self", st[0]);
        // many short threads: all end TERMINATED, none alive
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < 40; i++) {
            Thread t = new Thread(() -> {});
            ts.add(t);
            t.start();
        }
        int alive = 0;
        int term = 0;
        for (Thread t : ts) {
            t.join();
            if (t.isAlive()) {
                alive++;
            }
            if (t.getState() == Thread.State.TERMINATED) {
                term++;
            }
        }
        row("churn", alive + " " + term);
        // getAllStackTraces contains this thread
        row("allStackTraces.main", Thread.getAllStackTraces().containsKey(Thread.currentThread()));
        // thread group activeCount sanity
        row("group.main", Thread.currentThread().getThreadGroup().getName());
        // daemon after start throws
        Thread ds = new Thread(() -> {
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
            }
        });
        ds.start();
        try {
            ds.setDaemon(true);
            row("setDaemon.started", "no exception");
        } catch (IllegalThreadStateException e) {
            row("setDaemon.started", "ITSE " + e.getMessage());
        }
        ds.join();
        // Thread.Builder platform
        Thread pb = Thread.ofPlatform().name("pb-", 5).daemon(true).unstarted(() -> {});
        row("builder.platform", pb.getName() + " " + pb.isDaemon() + " " + pb.getState() + " " + pb.isAlive());
        pb.start();
        pb.join();
        row("builder.platform.after", pb.getState() + " " + pb.isAlive());
    }

    static List<String> runAll() throws Exception {
        out.clear();
        states();
        locals();
        virtuals();
        construction();
        handlers();
        misc();
        return new ArrayList<>(out);
    }

    public static void main(String[] args) throws Exception {
        List<String> first = runAll();
        for (String s : first) {
            System.out.println(s);
        }
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int drift = 0;
        for (int i = 0; i < reps; i++) {
            List<String> again = runAll();
            if (!again.equals(first)) {
                drift++;
                for (int k = 0; k < Math.max(first.size(), again.size()); k++) {
                    String a = k < first.size() ? first.get(k) : "-";
                    String b = k < again.size() ? again.get(k) : "-";
                    if (!a.equals(b)) {
                        System.out.println("DRIFT rep " + i + ": " + a + "  ->  " + b);
                    }
                }
            }
        }
        System.out.println("drift reps = " + drift);
    }
}
