// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.logging.Filter;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

/**
 * gci i25/jl (2026-10-15): {@code docs/known-issues/gc/gci-i24lk-jul-delivery-review-findings-20261015.md}
 * (F1 filter answer, F2 parent handlers), the level gate of every {@code log} entry point, a null
 * message, and the weak named-logger registry ({@code CRATONVM_JUL_WEAK_LOGGER_REGISTRY}).
 *
 * <p>Modes (one per run; stdout deterministic, identical on HotSpot 25):
 * <ul>
 *   <li>{@code filter}: a rejecting and then an accepting {@code Filter} on a logger with a counting
 *       handler, over seven entry points ({@code log(Level,String)}, {@code logp}, {@code info},
 *       {@code log(Level,String,Object)}, {@code log(Level,String,Object[])}, {@code entering},
 *       {@code log(LogRecord)}). HotSpot: the filter is asked once per call, a {@code false} stops
 *       delivery, and the filter sees the very record the handler gets.</li>
 *   <li>{@code parents}: {@code jl25.p} (handler P, no parent handlers) / {@code jl25.p.c} (handler
 *       C) / {@code jl25.p.c.d} (no handler). HotSpot publishes to every level until a level's
 *       {@code useParentHandlers} is false.</li>
 *   <li>{@code levels}: an INFO logger drops FINE on every entry point; an OFF logger drops even
 *       {@code Level.OFF}; an ALL logger takes {@code entering}.</li>
 *   <li>{@code nullmsg}: {@code log(SEVERE, (String) null)} and {@code logp(SEVERE, c, m, null)}
 *       deliver a record whose {@code getMessage()} is null.</li>
 *   <li>{@code weak N}: N named loggers nobody keeps; HotSpot's {@code LogManager} holds them weakly,
 *       so all N clear.</li>
 * </ul>
 * Usage: {@code GciJl25JulDeliveryProbe filter|parents|levels|nullmsg|weak [N]}.
 */
public final class GciJl25JulDeliveryProbe {

    /** Counts every record it is handed; remembers the last one. */
    static final class Counter extends Handler {
        int published;
        LogRecord last;
        final List<String> messages = new ArrayList<>();

        @Override
        public void publish(LogRecord record) {
            published++;
            last = record;
            messages.add(String.valueOf(record.getMessage()));
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    /** Answers {@code answer}; counts calls; remembers the last record. */
    static final class Gate implements Filter {
        boolean answer;
        int calls;
        LogRecord last;

        @Override
        public boolean isLoggable(LogRecord record) {
            calls++;
            last = record;
            return answer;
        }
    }

    static Logger quiet(String name) {
        Logger l = Logger.getLogger(name);
        l.setUseParentHandlers(false);
        return l;
    }

    /** One call through each of the seven entry points; returns how many records reached {@code h}. */
    static int sevenCalls(Logger l, Counter h, Gate g, int[] sameRecord) {
        int before = h.published;
        Runnable[] calls = {
            () -> l.log(Level.INFO, "jl25 log"),
            () -> l.logp(Level.INFO, "Jl25", "logp", "jl25 logp"),
            () -> l.info("jl25 info"),
            () -> l.log(Level.INFO, "jl25 one {0}", "p"),
            () -> l.log(Level.INFO, "jl25 many {0} {1}", new Object[] {"a", "b"}),
            () -> l.entering("Jl25", "entering"),
            () -> l.log(new LogRecord(Level.INFO, "jl25 record")),
        };
        for (Runnable call : calls) {
            int handlerBefore = h.published;
            call.run();
            if (h.published > handlerBefore && h.last == g.last) {
                sameRecord[0]++;
            }
        }
        return h.published - before;
    }

    static void filter() {
        Logger l = quiet("jl25.f");
        l.setLevel(Level.ALL);
        Counter h = new Counter();
        l.addHandler(h);
        Gate g = new Gate();
        l.setFilter(g);
        int[] same = {0};
        g.answer = false;
        int rejected = sevenCalls(l, h, g, same);
        int rejectCalls = g.calls;
        g.answer = true;
        g.calls = 0;
        int accepted = sevenCalls(l, h, g, same);
        System.out.println("jl25 mode=filter rejected=" + rejected + " rejectAsked=" + rejectCalls
                + " accepted=" + accepted + " acceptAsked=" + g.calls + " sameRecord=" + same[0]);
    }

    static void parents() {
        Logger p = quiet("jl25.p");
        Logger c = Logger.getLogger("jl25.p.c");
        Logger d = Logger.getLogger("jl25.p.c.d");
        Counter hp = new Counter();
        Counter hc = new Counter();
        p.addHandler(hp);
        c.addHandler(hc);
        c.info("from c");
        d.info("from d");
        d.log(Level.WARNING, "from d log");
        d.logp(Level.WARNING, "Jl25", "parents", "from d logp");
        String both = "c=" + hc.published + " p=" + hp.published;
        c.setUseParentHandlers(false);
        d.info("after");
        d.log(Level.WARNING, "after log");
        String stopped = "c=" + hc.published + " p=" + hp.published;
        System.out.println("jl25 mode=parents both[" + both + "] stopped[" + stopped + "]");
        // Keep the three loggers strongly reachable until here.
        if (p.getName().isEmpty() || c.getName().isEmpty() || d.getName().isEmpty()) {
            System.out.println("unreachable");
        }
    }

    static void levels() {
        Logger l = quiet("jl25.l");
        l.setLevel(Level.INFO);
        Counter h = new Counter();
        l.addHandler(h);
        l.fine("fine");
        l.log(Level.FINE, "log fine");
        l.logp(Level.FINE, "Jl25", "levels", "logp fine");
        l.log(Level.FINE, "log fine {0}", "p");
        l.log(Level.FINE, "log fine {0}", new Object[] {"p"});
        l.entering("Jl25", "levels");
        l.exiting("Jl25", "levels");
        l.throwing("Jl25", "levels", new RuntimeException("t"));
        int info = h.published;
        l.setLevel(Level.OFF);
        l.log(Level.OFF, "off at off");
        l.severe("severe at off");
        l.log(Level.SEVERE, "log severe at off");
        int off = h.published - info;
        l.setLevel(Level.ALL);
        l.entering("Jl25", "levels");
        l.log(Level.FINEST, "finest at all");
        int all = h.published - info - off;
        System.out.println("jl25 mode=levels info=" + info + " off=" + off + " all=" + all);
    }

    static void nullmsg() {
        Logger l = quiet("jl25.n");
        Counter h = new Counter();
        l.addHandler(h);
        l.log(Level.SEVERE, (String) null);
        l.logp(Level.SEVERE, "Jl25", "nullmsg", (String) null);
        System.out.println("jl25 mode=nullmsg published=" + h.published + " messages=" + h.messages);
    }

    static void weak(int n) throws InterruptedException {
        List<WeakReference<Logger>> refs = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            Logger l = Logger.getLogger("jl25.w" + i);
            l.setUseParentHandlers(false);
            l.addHandler(new Counter());
            refs.add(new WeakReference<>(l));
        }
        int cleared = 0;
        int rounds = 0;
        long t0 = System.nanoTime();
        for (; rounds < 40; rounds++) {
            System.gc();
            Thread.sleep(25);
            cleared = 0;
            for (WeakReference<Logger> r : refs) {
                if (r.get() == null) {
                    cleared++;
                }
            }
            if (cleared == n) {
                break;
            }
        }
        // A cleared name is minted afresh, usable as before.
        Logger again = Logger.getLogger("jl25.w0");
        Counter h = new Counter();
        again.setUseParentHandlers(false);
        again.addHandler(h);
        again.info("again");
        System.err.println("[jl25] weak rounds=" + rounds + " cleared=" + cleared + " ms="
                + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("jl25 mode=weak loggers=" + n + " cleared=" + cleared + " again=" + h.published);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "filter";
        switch (mode) {
            case "filter":
                filter();
                break;
            case "parents":
                parents();
                break;
            case "levels":
                levels();
                break;
            case "nullmsg":
                nullmsg();
                break;
            case "weak":
                weak(args.length > 1 ? Integer.parseInt(args[1]) : 32);
                break;
            default:
                System.out.println("usage: filter|parents|levels|nullmsg|weak [N]");
        }
    }
}
