// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L1:
// docs/internal/fixed-bugs/interpreter-i74-orch-compatible-anonymous-logger-parent-config-npe-FIXED-20261111.md
//
// `Logger.getAnonymousLogger()` runs the JDK body in both modes: the real
// `Logger` constructor, then `LogManager.getLogger("")` for the root and
// `doSetParent(root)`, whose `updateEffectiveLevel` reads
// `parent.config.levelValue`. Under `--compatible` the root comes from this
// VM's `LogManager` stand-in, which minted it without `config`, so the call
// died with `NullPointerException: Cannot read field "levelValue" because
// "this.parent.config" is null`. Each row is one anonymous-logger use.
//
// Run: `cratonvm L1W79AnonymousLogger` (jdk-only) and
// `cratonvm --compatible L1W79AnonymousLogger`; both must print the block
// below.
//
// The expected block is DERIVED from the JDK 25 sources (Logger,
// LogManager, the default logging.properties `.level= INFO`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// anonymous: name=null parent=[] level=null useParentHandlers=true
// anonymous loggable: INFO=true FINE=false
// anonymous(bundle null): parent=[]
// two anonymous: distinct=true same-parent=true
// handlers: added=1 removed=0
// done
// HOTSPOT_EXPECTED_END
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;

public class L1W79AnonymousLogger {
    static String row(java.util.concurrent.Callable<String> body) {
        try {
            return body.call();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        System.out.println("anonymous: " + row(() -> {
            Logger a = Logger.getAnonymousLogger();
            Logger p = a.getParent();
            return "name=" + a.getName() + " parent=[" + (p == null ? "null" : p.getName()) + "]"
                    + " level=" + a.getLevel() + " useParentHandlers=" + a.getUseParentHandlers();
        }));
        System.out.println("anonymous loggable: " + row(() -> {
            Logger a = Logger.getAnonymousLogger();
            return "INFO=" + a.isLoggable(Level.INFO) + " FINE=" + a.isLoggable(Level.FINE);
        }));
        System.out.println("anonymous(bundle null): " + row(() -> {
            Logger a = Logger.getAnonymousLogger((String) null);
            Logger p = a.getParent();
            return "parent=[" + (p == null ? "null" : p.getName()) + "]";
        }));
        System.out.println("two anonymous: " + row(() -> {
            Logger a = Logger.getAnonymousLogger();
            Logger b = Logger.getAnonymousLogger();
            return "distinct=" + (a != b) + " same-parent=" + (a.getParent() == b.getParent());
        }));
        System.out.println("handlers: " + row(() -> {
            Logger a = Logger.getAnonymousLogger();
            a.setUseParentHandlers(false);
            Handler h = new Handler() {
                @Override
                public void publish(LogRecord record) {
                }

                @Override
                public void flush() {
                }

                @Override
                public void close() {
                }
            };
            a.addHandler(h);
            int added = a.getHandlers().length;
            a.removeHandler(h);
            return "added=" + added + " removed=" + a.getHandlers().length;
        }));
        System.out.println("done");
    }
}
