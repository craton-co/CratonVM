// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

/**
 * gci i32/jh -- a JUL publish that fails surfaces its exception
 * ({@code i31-hp-report.md} review item 5). HotSpot 25's
 * {@code Logger.log(LogRecord)} calls {@code handler.publish} with no catch,
 * so a handler's {@code RuntimeException} leaves every logging overload that
 * reaches it ({@code log(Level, String, Object[])}, {@code log(Level, String,
 * Object)}, {@code log(Level, String, Throwable)}, {@code logp},
 * {@code entering}). Under {@code --compatible} CratonVM's natives swallowed it
 * on the parameterized and trace doors and printed a console line (or, after
 * i31/hp, nothing) instead. A null message on the parameterized door is
 * still published ({@code getMessage() == null}).
 *
 * <pre>
 *   java     -Xmx64m GciJh32JulFailureProbe
 *   cratonvm --compatible -Xmx64m GciJh32JulFailureProbe
 * </pre>
 *
 * stdout (HotSpot 25, deterministic): one line per door,
 * {@code <door> caught RuntimeException JH32-BOOM}, then
 * {@code nullmsg published 1 message null params 1}, then {@code done}.
 * stderr: nothing of the probe's own.
 */
public class GciJh32JulFailureProbe {
    static final class Boom extends Handler {
        @Override
        public void publish(LogRecord record) {
            throw new RuntimeException("JH32-BOOM");
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    static final class Keep extends Handler {
        final List<LogRecord> records = new ArrayList<>();

        @Override
        public void publish(LogRecord record) {
            records.add(record);
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    }

    interface Door {
        void run(Logger logger);
    }

    static void door(Logger logger, String name, Door door) {
        String line;
        try {
            door.run(logger);
            line = name + " no exception";
        } catch (RuntimeException e) {
            line = name + " caught " + e.getClass().getSimpleName() + " " + e.getMessage();
        }
        System.out.println(line);
    }

    public static void main(String[] args) {
        Logger boom = Logger.getLogger("gcijh32.boom");
        boom.setUseParentHandlers(false);
        boom.setLevel(Level.ALL);
        Handler h = new Boom();
        h.setLevel(Level.ALL);
        boom.addHandler(h);

        door(boom, "log-params", l -> l.log(Level.INFO, "p {0}", new Object[] {"x"}));
        door(boom, "log-param1", l -> l.log(Level.INFO, "p {0}", "x"));
        door(boom, "log-thrown", l -> l.log(Level.INFO, "t", new IllegalStateException("inner")));
        door(boom, "logp", l -> l.logp(Level.INFO, "C", "m", "lp"));
        door(boom, "entering", l -> l.entering("C", "m"));
        door(boom, "info", l -> l.info("i"));

        Logger keep = Logger.getLogger("gcijh32.keep");
        keep.setUseParentHandlers(false);
        Keep k = new Keep();
        keep.addHandler(k);
        keep.log(Level.INFO, (String) null, new Object[] {"x"});
        LogRecord r = k.records.isEmpty() ? null : k.records.get(0);
        System.out.println("nullmsg published " + k.records.size()
                + " message " + (r == null ? "-" : String.valueOf(r.getMessage()))
                + " params " + (r == null || r.getParameters() == null ? "-" : String.valueOf(r.getParameters().length)));
        System.out.println("done");
    }
}
