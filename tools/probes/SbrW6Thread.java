// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

/**
 * SBR wave 6, lane {@code thread}: the {@code java.lang.Thread} rows
 * ({@code RETIRED_SHADOW_SBR_THREAD_TRIPLES}) -- accessors whose state lives in
 * the real {@code Thread}/{@code FieldHolder} fields, the interrupt protocol,
 * and the lifecycle rows.
 *
 * <p>Each row returns deterministic observations only (no thread number, no
 * identity hash, no timing but coarse flags). It is evaluated ONCE and printed,
 * then re-evaluated in a hot loop and compared with the first value; {@code
 * drift} counts mismatches. Diff the whole output against HotSpot 25.
 *
 * <pre>
 *   java SbrW6Thread [iters]              (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW6Thread [iters]
 * </pre>
 */
public class SbrW6Thread {
    interface S {
        String get() throws Exception;
    }

    static final List<String> LABELS = new ArrayList<>();
    static final List<S> ROWS = new ArrayList<>();

    static void row(String label, S s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static final String TIMEOUT = "!TIMEOUT (row did not finish in 20 s)";

    static String safe(S s) {
        final String[] out = {TIMEOUT};
        Thread t = new Thread(() -> {
            String v;
            try {
                v = s.get();
            } catch (Throwable e) {
                v = "!" + e.getClass().getName() + ": " + e.getMessage();
            }
            out[0] = v;
        });
        t.setDaemon(true);
        t.start();
        try {
            t.join(20_000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        return t.isAlive() ? TIMEOUT : out[0];
    }

    static String ex(S s) {
        try {
            return "ok:" + s.get();
        } catch (Throwable t) {
            return "ex:" + t.getClass().getName() + ":" + t.getMessage();
        }
    }

    /** A thread name with the JDK's "Thread-N" numbering normalised away. */
    static String nm(String n) {
        return n == null ? "null" : n.replaceAll("^Thread-[0-9]+$", "Thread-N");
    }

    /** Wait until {@code t} reports one of the given states (bounded). */
    static Thread.State awaitState(Thread t, Thread.State... any) throws Exception {
        for (int i = 0; i < 4000; i++) {
            Thread.State s = t.getState();
            for (Thread.State w : any) if (s == w) return s;
            Thread.sleep(2);
        }
        return t.getState();
    }

    static final Object LOCK = new Object();

    public static void main(String[] args) throws Exception {
        // ---- names ------------------------------------------------------------------------
        row("name.ctors", () -> {
            Runnable r = () -> {};
            ThreadGroup g = new ThreadGroup("g6");
            return nm(new Thread().getName()) + " " + nm(new Thread(r).getName())
                    + " " + new Thread("a").getName() + " " + new Thread(r, "b").getName()
                    + " " + new Thread(g, r, "c").getName() + " " + new Thread(g, "d").getName()
                    + " " + new Thread(g, r, "e", 0L).getName() + " " + new Thread(g, r, "f", 0L, false).getName()
                    + " " + nm(new Thread(g, r).getName());
        });
        row("name.null", () -> ex(() -> new Thread((String) null).getName())
                + " " + ex(() -> new Thread(() -> {}, null).getName())
                + " " + ex(() -> new Thread(new ThreadGroup("x"), () -> {}, null).getName()));
        row("name.set", () -> {
            Thread t = new Thread("before");
            t.setName("after");
            String a = t.getName();
            String n = ex(() -> {
                t.setName(null);
                return "?";
            });
            return a + " " + t.getName() + " " + n;
        });
        row("name.setStarted", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread t = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            }, "s0");
            t.start();
            t.setName("s1");
            String r = t.getName();
            go.countDown();
            t.join();
            t.setName("s2");
            return r + " " + t.getName();
        });
        row("name.current", () -> {
            Thread c = Thread.currentThread();
            String old = c.getName();
            c.setName("renamed-current");
            String r = Thread.currentThread().getName();
            c.setName(old);
            return r + " " + (c.getName().equals(old));
        });
        row("name.subclass", () -> {
            Thread t = new Thread() {
                @Override
                public void run() {
                }
            };
            return nm(t.getName()) + " " + t.getClass().getSuperclass().getName();
        });
        row("name.main", () -> "" + Thread.currentThread().getName().startsWith("Thread-"));

        // ---- priority / daemon / group ----------------------------------------------------
        row("prio.default", () -> {
            Thread t = new Thread(() -> {});
            return t.getPriority() + " " + Thread.MIN_PRIORITY + " " + Thread.MAX_PRIORITY;
        });
        row("prio.set", () -> {
            Thread t = new Thread(() -> {});
            t.setPriority(3);
            String a = "" + t.getPriority();
            t.setPriority(Thread.MAX_PRIORITY);
            return a + " " + t.getPriority() + " " + ex(() -> {
                t.setPriority(0);
                return "?";
            }) + " " + ex(() -> {
                t.setPriority(11);
                return "?";
            });
        });
        row("prio.group", () -> {
            ThreadGroup g = new ThreadGroup("low");
            g.setMaxPriority(3);
            Thread t = new Thread(g, () -> {}, "p");
            String a = "" + t.getPriority();
            t.setPriority(9);
            return a + " " + t.getPriority() + " " + g.getMaxPriority();
        });
        row("prio.inherit", () -> {
            int[] seen = new int[2];
            Thread p = new Thread(() -> {
                Thread c = new Thread(() -> {});
                seen[0] = c.getPriority();
            });
            p.setPriority(7);
            p.start();
            p.join();
            return "child=" + seen[0];
        });
        row("prio.virtual", () -> {
            Thread v = Thread.ofVirtual().unstarted(() -> {});
            v.setPriority(2);
            return v.getPriority() + " " + ex(() -> {
                v.setPriority(0);
                return "?";
            });
        });
        row("daemon.basic", () -> {
            Thread t = new Thread(() -> {});
            String a = "" + t.isDaemon();
            t.setDaemon(true);
            String b = "" + t.isDaemon();
            t.setDaemon(false);
            return a + " " + b + " " + t.isDaemon();
        });
        row("daemon.inherit", () -> {
            boolean[] seen = new boolean[2];
            Thread p = new Thread(() -> {
                seen[0] = new Thread(() -> {}).isDaemon();
            });
            p.setDaemon(true);
            p.start();
            p.join();
            Thread q = new Thread(() -> {
                seen[1] = new Thread(() -> {}).isDaemon();
            });
            q.start();
            q.join();
            return seen[0] + " " + seen[1];
        });
        row("daemon.afterStart", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread t = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            });
            t.start();
            String r = ex(() -> {
                t.setDaemon(true);
                return "?";
            });
            go.countDown();
            t.join();
            return r + " " + t.isDaemon() + " " + ex(() -> {
                t.setDaemon(true);
                return "" + t.isDaemon();
            });
        });
        row("daemon.virtual", () -> {
            Thread v = Thread.ofVirtual().unstarted(() -> {});
            return v.isDaemon() + " " + ex(() -> {
                v.setDaemon(false);
                return "?";
            }) + " " + ex(() -> {
                v.setDaemon(true);
                return "" + v.isDaemon();
            });
        });
        row("group.basic", () -> {
            ThreadGroup g = new ThreadGroup("gg");
            Thread t = new Thread(g, () -> {}, "t");
            Thread u = new Thread(() -> {});
            return t.getThreadGroup().getName() + " " + (u.getThreadGroup() == Thread.currentThread().getThreadGroup())
                    + " " + Thread.currentThread().getThreadGroup().getName();
        });
        row("group.terminated", () -> {
            ThreadGroup g = new ThreadGroup("gt");
            Thread t = new Thread(g, () -> {}, "t");
            String a = t.getThreadGroup().getName();
            t.start();
            t.join();
            return a + " " + t.getThreadGroup();
        });
        row("group.virtual", () -> {
            Thread v = Thread.ofVirtual().unstarted(() -> {});
            ThreadGroup g = v.getThreadGroup();
            return g == null ? "null" : g.getName() + " " + g.getMaxPriority();
        });
        row("group.virtualDone", () -> {
            Thread v = Thread.ofVirtual().start(() -> {});
            v.join();
            return "" + v.getThreadGroup();
        });

        // ---- context class loader ---------------------------------------------------------
        row("ccl.inherit", () -> {
            ClassLoader mine = Thread.currentThread().getContextClassLoader();
            Thread t = new Thread(() -> {});
            Thread w = new Thread(new ThreadGroup("cg"), () -> {}, "w", 0L, false);
            return (t.getContextClassLoader() == mine) + " " + (w.getContextClassLoader() == ClassLoader.getSystemClassLoader());
        });
        row("ccl.set", () -> {
            Thread t = new Thread(() -> {});
            ClassLoader cl = new ClassLoader() {
            };
            t.setContextClassLoader(cl);
            String a = "" + (t.getContextClassLoader() == cl);
            t.setContextClassLoader(null);
            return a + " " + t.getContextClassLoader();
        });
        row("ccl.child", () -> {
            ClassLoader cl = new ClassLoader() {
            };
            ClassLoader[] seen = new ClassLoader[1];
            Thread p = new Thread(() -> {
                Thread c = new Thread(() -> seen[0] = Thread.currentThread().getContextClassLoader());
                c.start();
                try {
                    c.join();
                } catch (InterruptedException e) {
                }
            });
            p.setContextClassLoader(cl);
            p.start();
            p.join();
            return "" + (seen[0] == cl);
        });
        row("ccl.main", () -> "" + (Thread.currentThread().getContextClassLoader() == ClassLoader.getSystemClassLoader()));

        // ---- uncaught exception handler ---------------------------------------------------
        row("ueh.default", () -> {
            Thread t = new Thread(() -> {});
            return (t.getUncaughtExceptionHandler() == t.getThreadGroup()) + " " + Thread.getDefaultUncaughtExceptionHandler();
        });
        row("ueh.set", () -> {
            Thread t = new Thread(() -> {});
            Thread.UncaughtExceptionHandler h = (th, e) -> {};
            t.setUncaughtExceptionHandler(h);
            String a = "" + (t.getUncaughtExceptionHandler() == h);
            t.setUncaughtExceptionHandler(null);
            return a + " " + (t.getUncaughtExceptionHandler() == t.getThreadGroup());
        });
        row("ueh.fires", () -> {
            String[] seen = new String[1];
            Thread t = new Thread(() -> {
                throw new IllegalStateException("boom");
            }, "u1");
            t.setUncaughtExceptionHandler((th, e) -> seen[0] = th.getName() + ":" + e.getMessage());
            t.start();
            t.join();
            return seen[0];
        });
        row("ueh.terminated", () -> {
            Thread t = new Thread(() -> {});
            t.setUncaughtExceptionHandler((th, e) -> {});
            t.start();
            t.join();
            return "" + t.getUncaughtExceptionHandler();
        });
        row("ueh.default.fires", () -> {
            Thread.UncaughtExceptionHandler old = Thread.getDefaultUncaughtExceptionHandler();
            String[] seen = new String[1];
            try {
                Thread.setDefaultUncaughtExceptionHandler((th, e) -> seen[0] = th.getName() + ":" + e.getMessage());
                Thread t = new Thread(() -> {
                    throw new IllegalStateException("dflt");
                }, "u2");
                t.start();
                t.join();
            } finally {
                Thread.setDefaultUncaughtExceptionHandler(old);
            }
            return seen[0];
        });
        row("ueh.group", () -> {
            String[] seen = new String[1];
            ThreadGroup g = new ThreadGroup("ug") {
                @Override
                public void uncaughtException(Thread t, Throwable e) {
                    seen[0] = t.getName() + ":" + e.getClass().getSimpleName();
                }
            };
            Thread t = new Thread(g, () -> {
                throw new RuntimeException("x");
            }, "u3");
            t.start();
            t.join();
            return seen[0];
        });

        // ---- interrupts -------------------------------------------------------------------
        row("intr.self", () -> {
            Thread c = Thread.currentThread();
            String a = "" + c.isInterrupted();
            c.interrupt();
            String b = "" + c.isInterrupted();
            String d = "" + Thread.interrupted();
            return a + " " + b + " " + d + " " + c.isInterrupted() + " " + Thread.interrupted();
        });
        row("intr.isInterruptedNoClear", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            String r = c.isInterrupted() + " " + c.isInterrupted();
            Thread.interrupted();
            return r + " " + c.isInterrupted();
        });
        row("intr.unstarted", () -> {
            Thread t = new Thread(() -> {});
            String a = "" + t.isInterrupted();
            t.interrupt();
            String b = "" + t.isInterrupted();
            t.start();
            t.join();
            return a + " " + b + " " + t.isInterrupted();
        });
        row("intr.terminated", () -> {
            Thread t = new Thread(() -> {});
            t.start();
            t.join();
            String a = "" + t.isInterrupted();
            t.interrupt();
            return a + " " + t.isInterrupted();
        });
        row("intr.sleep", () -> {
            String[] seen = new String[1];
            CountDownLatch in = new CountDownLatch(1);
            Thread t = new Thread(() -> {
                in.countDown();
                try {
                    Thread.sleep(60_000);
                    seen[0] = "slept";
                } catch (InterruptedException e) {
                    seen[0] = "IE isInterrupted=" + Thread.currentThread().isInterrupted()
                            + " msg=" + e.getMessage();
                }
            });
            t.start();
            in.await();
            awaitState(t, Thread.State.TIMED_WAITING);
            t.interrupt();
            t.join();
            return seen[0] + " after=" + t.isInterrupted();
        });
        row("intr.sleepPreInterrupted", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            String r = ex(() -> {
                Thread.sleep(50);
                return "slept";
            });
            return r + " " + c.isInterrupted();
        });
        row("intr.sleepDuration", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            String r = ex(() -> {
                Thread.sleep(Duration.ofMillis(50));
                return "slept";
            });
            return r + " " + c.isInterrupted();
        });
        row("intr.wait", () -> {
            String[] seen = new String[1];
            Thread t = new Thread(() -> {
                synchronized (LOCK) {
                    try {
                        LOCK.wait();
                        seen[0] = "woke";
                    } catch (InterruptedException e) {
                        seen[0] = "IE " + Thread.currentThread().isInterrupted();
                    }
                }
            });
            t.start();
            awaitState(t, Thread.State.WAITING);
            t.interrupt();
            t.join();
            return seen[0];
        });
        row("intr.join", () -> {
            String[] seen = new String[1];
            Thread sleeper = new Thread(() -> {
                try {
                    Thread.sleep(200);
                } catch (InterruptedException e) {
                }
            });
            sleeper.start();
            Thread t = new Thread(() -> {
                try {
                    sleeper.join();
                    seen[0] = "joined";
                } catch (InterruptedException e) {
                    seen[0] = "IE " + Thread.currentThread().isInterrupted();
                }
            });
            t.start();
            awaitState(t, Thread.State.WAITING);
            t.interrupt();
            t.join();
            sleeper.join();
            return seen[0];
        });
        row("intr.park", () -> {
            boolean[] seen = new boolean[3];
            Thread t = new Thread(() -> {
                LockSupport.park();
                seen[0] = Thread.currentThread().isInterrupted();
                LockSupport.park(); // interrupt status still set: returns at once
                seen[1] = Thread.interrupted();
                seen[2] = Thread.currentThread().isInterrupted();
            });
            t.start();
            awaitState(t, Thread.State.WAITING);
            t.interrupt();
            t.join();
            return seen[0] + " " + seen[1] + " " + seen[2];
        });
        row("intr.parkNanos", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            long t0 = System.nanoTime();
            LockSupport.parkNanos(5_000_000_000L);
            boolean quick = System.nanoTime() - t0 < 2_000_000_000L;
            return quick + " " + Thread.interrupted();
        });
        row("intr.otherThreadFlag", () -> {
            CountDownLatch go = new CountDownLatch(1);
            boolean[] seen = new boolean[2];
            Thread t = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    seen[0] = Thread.currentThread().isInterrupted();
                }
                seen[1] = Thread.currentThread().isInterrupted();
            });
            t.start();
            awaitState(t, Thread.State.WAITING);
            String before = "" + t.isInterrupted();
            t.interrupt();
            t.join();
            return before + " " + seen[0] + " " + seen[1];
        });
        row("intr.awaitLatchPending", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            String r = ex(() -> {
                new CountDownLatch(1).await(1, TimeUnit.SECONDS);
                return "await";
            });
            return r + " " + c.isInterrupted();
        });
        row("intr.reinterrupt", () -> {
            Thread c = Thread.currentThread();
            c.interrupt();
            c.interrupt();
            String a = "" + Thread.interrupted();
            String b = "" + Thread.interrupted();
            return a + " " + b;
        });
        row("intr.virtual", () -> {
            String[] seen = new String[1];
            CountDownLatch in = new CountDownLatch(1);
            Thread v = Thread.ofVirtual().unstarted(() -> {
                in.countDown();
                try {
                    Thread.sleep(60_000);
                    seen[0] = "slept";
                } catch (InterruptedException e) {
                    seen[0] = "IE " + Thread.currentThread().isInterrupted();
                }
            });
            v.start();
            in.await();
            awaitState(v, Thread.State.TIMED_WAITING);
            v.interrupt();
            v.join();
            return seen[0];
        });

        // ---- lifecycle --------------------------------------------------------------------
        row("alive.basic", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread t = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            });
            String a = "" + t.isAlive();
            t.start();
            String b = "" + t.isAlive();
            go.countDown();
            t.join();
            return a + " " + b + " " + t.isAlive() + " " + Thread.currentThread().isAlive();
        });
        row("alive.virtual", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread v = Thread.ofVirtual().unstarted(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            });
            String a = "" + v.isAlive();
            v.start();
            String b = "" + v.isAlive();
            go.countDown();
            v.join();
            return a + " " + b + " " + v.isAlive();
        });
        row("state.basic", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread t = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            });
            String a = "" + t.getState();
            t.start();
            String b = "" + awaitState(t, Thread.State.WAITING);
            go.countDown();
            t.join();
            return a + " " + b + " " + t.getState();
        });
        row("state.timed", () -> {
            Thread t = new Thread(() -> {
                try {
                    Thread.sleep(300);
                } catch (InterruptedException e) {
                }
            });
            t.start();
            String a = "" + awaitState(t, Thread.State.TIMED_WAITING);
            t.join();
            return a;
        });
        row("state.blocked", () -> {
            Thread t = new Thread(() -> {
                synchronized (LOCK) {
                }
            });
            String a;
            synchronized (LOCK) {
                t.start();
                a = "" + awaitState(t, Thread.State.BLOCKED);
            }
            t.join();
            return a + " " + t.getState();
        });
        row("state.runnable", () -> {
            boolean[] stop = new boolean[1];
            Thread t = new Thread(() -> {
                while (!stop[0]) {
                    Thread.onSpinWait();
                }
            });
            t.start();
            Thread.sleep(20);
            String a = "" + t.getState();
            stop[0] = true;
            t.join();
            return a + " " + Thread.currentThread().getState();
        });
        row("state.park", () -> {
            Thread t = new Thread(() -> LockSupport.park());
            t.start();
            String a = "" + awaitState(t, Thread.State.WAITING);
            LockSupport.unpark(t);
            t.join();
            return a;
        });
        row("state.virtual", () -> {
            CountDownLatch go = new CountDownLatch(1);
            Thread v = Thread.ofVirtual().unstarted(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                }
            });
            String a = "" + v.getState();
            v.start();
            String b = "" + awaitState(v, Thread.State.WAITING);
            go.countDown();
            v.join();
            return a + " " + b + " " + v.getState();
        });
        row("start.twice", () -> {
            Thread t = new Thread(() -> {});
            t.start();
            String a = ex(() -> {
                t.start();
                return "?";
            });
            t.join();
            return a + " " + ex(() -> {
                t.start();
                return "?";
            });
        });
        row("start.virtualTwice", () -> {
            Thread v = Thread.ofVirtual().unstarted(() -> {});
            v.start();
            String a = ex(() -> {
                v.start();
                return "?";
            });
            v.join();
            return a;
        });
        row("run.direct", () -> {
            String[] seen = new String[1];
            Thread t = new Thread(() -> seen[0] = Thread.currentThread().getName(), "direct");
            t.run();
            return nm(seen[0]) + " " + t.getState() + " " + t.isAlive() + " " + ex(() -> {
                new Thread((Runnable) null).run();
                return "nulltask";
            });
        });
        row("run.subclass", () -> {
            String[] seen = new String[2];
            Thread t = new Thread(() -> seen[0] = "target") {
                @Override
                public void run() {
                    seen[1] = "override";
                    super.run();
                }
            };
            t.start();
            t.join();
            return seen[0] + " " + seen[1];
        });
        row("run.throws", () -> {
            Thread t = new Thread(() -> {
                throw new IllegalArgumentException("run-throws");
            });
            t.setUncaughtExceptionHandler((th, e) -> {});
            return ex(() -> {
                t.run();
                return "returned";
            });
        });
        row("run.nested", () -> {
            StringBuilder sb = new StringBuilder();
            Thread t = new Thread(() -> {
                Thread u = new Thread(() -> sb.append("inner"));
                u.start();
                try {
                    u.join();
                } catch (InterruptedException e) {
                }
                sb.append("+outer");
            });
            t.start();
            t.join();
            return sb.toString();
        });
        row("sleep.args", () -> ex(() -> {
            Thread.sleep(-1);
            return "?";
        }) + " " + ex(() -> {
            Thread.sleep(0);
            return "zero";
        }) + " " + ex(() -> {
            Thread.sleep(1, 999_999);
            return "ns";
        }) + " " + ex(() -> {
            Thread.sleep(1, 1_000_000);
            return "?";
        }) + " " + ex(() -> {
            Thread.sleep(Duration.ofMillis(-5));
            return "negdur";
        }));
        row("sleep.timing", () -> {
            long t0 = System.nanoTime();
            Thread.sleep(40);
            long ms = (System.nanoTime() - t0) / 1_000_000;
            return (ms >= 39) + " " + (ms < 2000);
        });
        row("yield.basic", () -> {
            Thread.yield();
            boolean[] ok = new boolean[1];
            Thread v = Thread.ofVirtual().start(() -> {
                Thread.yield();
                ok[0] = true;
            });
            v.join();
            return "" + ok[0];
        });
        row("yield.virtualInterleave", () -> {
            StringBuffer sb = new StringBuffer();
            Thread a = Thread.ofVirtual().unstarted(() -> {
                sb.append("a1");
                Thread.yield();
                sb.append("a2");
            });
            a.start();
            a.join();
            return sb.toString();
        });
        row("tid.unique", () -> {
            Thread a = new Thread(() -> {});
            Thread b = new Thread(() -> {});
            return "" + (a.threadId() != b.threadId() && a.getId() == a.threadId() && a.threadId() > 0
                    && b.threadId() > a.threadId());
        });
        row("toString.shape", () -> {
            Thread t = new Thread(new ThreadGroup("tg"), () -> {}, "tname");
            t.setPriority(4);
            String s = t.toString().replaceAll("#[0-9]+", "#N");
            Thread v = Thread.ofVirtual().name("vname").unstarted(() -> {});
            return s + " | " + v.toString().replaceAll("@[0-9a-f]+", "@H").replaceAll("#[0-9]+", "#N");
        });
        row("ofPlatform.builder", () -> {
            Thread t = Thread.ofPlatform().name("bp-", 5).daemon(true).priority(2).unstarted(() -> {});
            return t.getName() + " " + t.isDaemon() + " " + t.getPriority() + " " + t.getState();
        });
        row("ofVirtual.basics", () -> {
            Thread v = Thread.ofVirtual().name("vv").unstarted(() -> {});
            String a = v.isVirtual() + " " + v.getName() + " " + v.isDaemon() + " " + v.getState();
            v.start();
            v.join();
            return a + " " + v.getState();
        });
        row("itl.inherit", () -> {
            InheritableThreadLocal<String> itl = new InheritableThreadLocal<>();
            itl.set("parent");
            String[] seen = new String[3];
            Thread t = new Thread(() -> seen[0] = itl.get());
            itl.set("later");
            t.start();
            t.join();
            Thread w = new Thread(new ThreadGroup("ig"), () -> seen[1] = itl.get(), "w", 0L, false);
            w.start();
            w.join();
            Thread u = new Thread(new ThreadGroup("ig2"), () -> seen[2] = itl.get(), "u", 0L, true);
            u.start();
            u.join();
            return seen[0] + " " + seen[1] + " " + seen[2];
        });
        row("holdsLock.basic", () -> {
            synchronized (LOCK) {
                return Thread.holdsLock(LOCK) + " " + Thread.holdsLock(new Object());
            }
        });
        row("many.threads", () -> {
            int n = 32;
            int[] sum = new int[1];
            Thread[] ts = new Thread[n];
            for (int i = 0; i < n; i++) {
                final int k = i;
                ts[i] = new Thread(() -> {
                    synchronized (sum) {
                        sum[0] += k;
                    }
                });
                ts[i].start();
            }
            for (Thread t : ts) t.join();
            return "" + sum[0];
        });

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                if (first[i] == TIMEOUT) continue;
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
