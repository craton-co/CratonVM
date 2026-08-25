package io.netty.handler.ssl;

import io.netty.channel.Channel;
import io.netty.channel.ChannelFuture;
import io.netty.util.concurrent.EventExecutor;
import io.netty.util.concurrent.Future;
import io.netty.util.concurrent.SingleThreadEventExecutor;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Java-side instrument for the `ParameterizedSslHandlerTest` residual stalls.
 *
 * The VM-side watchdog can say that a thread is parked in `Object.wait()` on a
 * promise that is still pending. It cannot say WHY the promise is pending,
 * because the answer is netty state: which channel the operation belongs to,
 * whether that channel is still open and registered, and — the one that
 * decides it — how many tasks are sitting in the owning event loop's queue.
 *
 * A close whose promise never completes has exactly two shapes:
 *
 *  * `pendingTasks > 0` while the loop is asleep — the task was submitted and
 *    the reactor was never woken for it. That is a lost `Selector.wakeup()`,
 *    and it is a VM defect.
 *  * `pendingTasks == 0` — the task RAN, and the promise it should have
 *    completed was not the one being awaited. That is a different defect and a
 *    different suspect list.
 *
 * Everything here is registration + printing; no test-visible behaviour
 * changes. `await` calls exactly the `syncUninterruptibly()` the test called.
 */
public final class PshProbe {
    private PshProbe() { }

    private static final long T0 = System.nanoTime();
    private static final AtomicLong SEQ = new AtomicLong();
    /** key -> {startNanos, subject}. */
    private static final Map<String, Object[]> OUTSTANDING = new ConcurrentHashMap<String, Object[]>();
    /** Report an outstanding operation once it has been stuck this long. */
    private static final long STUCK_MS = Long.getLong("psh.probe.stuckMs", 15000L);
    /** Halt the JVM once one operation has been outstanding this long. */
    private static final long FATAL_MS = Long.getLong("psh.probe.fatalMs", 120000L);

    static {
        Thread t = new Thread(new Runnable() {
            @Override
            public void run() {
                while (true) {
                    try {
                        Thread.sleep(5000L);
                    } catch (InterruptedException e) {
                        return;
                    }
                    for (Map.Entry<String, Object[]> e : OUTSTANDING.entrySet()) {
                        Object[] v = e.getValue();
                        long sinceMs = (System.nanoTime() - ((Long) v[0]).longValue()) / 1000000L;
                        if (sinceMs < STUCK_MS) {
                            continue;
                        }
                        describe(e.getKey(), sinceMs, v[1]);
                        if (sinceMs >= FATAL_MS) {
                            log("FATAL: " + e.getKey() + " outstanding for " + sinceMs
                                    + "ms — this is a hang, not slowness; halting so the"
                                    + " loop can move on");
                            System.err.flush();
                            System.out.flush();
                            Runtime.getRuntime().halt(97);
                        }
                    }
                }
            }
        }, "psh-probe-watchdog");
        t.setDaemon(true);
        t.start();
    }

    public static void log(String msg) {
        System.err.println("[PSH " + ((System.nanoTime() - T0) / 1000000L) + "ms "
                + Thread.currentThread().getName() + "] " + msg);
        System.err.flush();
    }

    public static String tag(String what) {
        return what + "#" + SEQ.incrementAndGet();
    }

    /** Await a channel future exactly as the test did, with the wait registered. */
    public static void await(String what, ChannelFuture f) {
        String key = tag(what);
        OUTSTANDING.put(key, new Object[] { Long.valueOf(System.nanoTime()), f });
        try {
            f.syncUninterruptibly();
        } finally {
            OUTSTANDING.remove(key);
        }
    }

    /** Register a promise wait without changing how the test waits on it. */
    public static String enter(String what, Object subject) {
        String key = tag(what);
        OUTSTANDING.put(key, new Object[] { Long.valueOf(System.nanoTime()), subject });
        return key;
    }

    public static void leave(String key) {
        OUTSTANDING.remove(key);
    }

    private static void describe(String key, long sinceMs, Object subject) {
        StringBuilder sb = new StringBuilder(256);
        sb.append("STUCK ").append(key).append(' ').append(sinceMs).append("ms");
        Channel ch = null;
        if (subject instanceof ChannelFuture) {
            ChannelFuture f = (ChannelFuture) subject;
            ch = f.channel();
            sb.append(" future.isDone=").append(f.isDone());
        } else if (subject instanceof Future) {
            sb.append(" future.isDone=").append(((Future<?>) subject).isDone());
        } else if (subject instanceof Channel) {
            ch = (Channel) subject;
        }
        if (subject instanceof Object[] && ((Object[]) subject).length == 2) {
            // {Future, Channel} pair — a promise whose owning loop we know.
            Object[] pair = (Object[]) subject;
            sb.append(" future.isDone=").append(((Future<?>) pair[0]).isDone());
            ch = (Channel) pair[1];
        }
        if (ch != null) {
            sb.append(" ch=").append(ch.getClass().getSimpleName())
              .append(" open=").append(ch.isOpen())
              .append(" active=").append(ch.isActive())
              .append(" registered=").append(ch.isRegistered());
            describeExecutor(sb, ch.eventLoop());
        }
        log(sb.toString());
        dumpReactors();
    }

    /** Print every netty reactor thread's stack, once per stuck report. */
    private static void dumpReactors() {
        try {
            for (Map.Entry<Thread, StackTraceElement[]> e : Thread.getAllStackTraces().entrySet()) {
                Thread t = e.getKey();
                String n = t.getName();
                if (n == null || !(n.startsWith("multiThreadIoEventLoopGroup")
                        || n.startsWith("globalEventExecutor")
                        || n.startsWith("nioEventLoopGroup"))) {
                    continue;
                }
                StringBuilder sb = new StringBuilder(512);
                sb.append("  reactor ").append(n).append(' ').append(t.getState())
                  .append(" alive=").append(t.isAlive()).append(':');
                StackTraceElement[] st = e.getValue();
                int limit = st.length < 12 ? st.length : 12;
                for (int i = 0; i < limit; i++) {
                    sb.append("\n      at ").append(st[i]);
                }
                if (st.length == 0) {
                    sb.append("\n      <no frames>");
                }
                log(sb.toString());
            }
        } catch (Throwable t) {
            log("  reactor dump failed: " + t);
        }
    }

    private static void describeExecutor(StringBuilder sb, EventExecutor ex) {
        if (ex == null) {
            sb.append(" loop=<null>");
            return;
        }
        sb.append(" loop=").append(Integer.toHexString(System.identityHashCode(ex)))
          .append(" inEventLoop=").append(ex.inEventLoop())
          .append(" shuttingDown=").append(ex.isShuttingDown());
        if (ex instanceof SingleThreadEventExecutor) {
            SingleThreadEventExecutor st = (SingleThreadEventExecutor) ex;
            // THE DECIDING NUMBER. A queued task on a reactor nobody woke is a
            // lost wakeup; an empty queue means the task ran and completed
            // something other than what is being awaited.
            sb.append(" pendingTasks=").append(st.pendingTasks());
            try {
                io.netty.util.concurrent.ThreadProperties tp = st.threadProperties();
                sb.append(" ownerState=").append(tp.state())
                  .append(" ownerAlive=").append(tp.isAlive())
                  .append(" ownerInterrupted=").append(tp.isInterrupted());
            } catch (Throwable ignore) {
                // threadProperties() throws if the loop never started.
                sb.append(" ownerState=<unstarted>");
            }
        }
    }
}
