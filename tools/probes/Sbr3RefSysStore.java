// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Hashtable;
import java.util.concurrent.Callable;
import java.util.logging.LogManager;
import javax.naming.Context;
import javax.naming.InitialContext;
import javax.naming.spi.InitialContextFactory;

/**
 * SBR round 3, lane {@code ref}: do natives that read a system property from the VM's own property
 * store see a write made through {@code System.setProperty} / the held {@code Properties} object?
 * Each row sets a property at run time and then triggers a consumer that reads it live on HotSpot.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefSysStore
 *   # control: CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/System.getProperties,java/lang/System.setProperty,java/lang/System.clearProperty
 * </pre>
 */
public final class Sbr3RefSysStore {
    public static class MyLogManager extends LogManager {
        public MyLogManager() {
            System.out.println("MyLogManager constructed");
        }
    }

    public static class Fact implements InitialContextFactory {
        @Override
        public Context getInitialContext(Hashtable<?, ?> env) {
            System.out.println("Fact.getInitialContext called");
            return null;
        }
    }

    static void row(String name, Callable<Object> s) {
        try {
            System.out.println(name + " = " + s.call());
        } catch (Throwable t) {
            System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        // 1. through setProperty
        row("jndi via setProperty", () -> {
            System.setProperty("java.naming.factory.initial", "Sbr3RefSysStore$Fact");
            new InitialContext();
            return "constructed";
        });
        System.clearProperty("java.naming.factory.initial");
        // 2. through the held Properties object
        row("jndi via held Properties", () -> {
            System.getProperties().setProperty("java.naming.factory.initial", "Sbr3RefSysStore$Fact");
            new InitialContext();
            return "constructed";
        });
        System.getProperties().remove("java.naming.factory.initial");
        row("jndi after removal", () -> {
            try {
                new InitialContext().lookup("x");
                return "found";
            } catch (javax.naming.NamingException e) {
                return e.getClass().getSimpleName();
            }
        });
        // 3. log manager (read once, at first use)
        row("logmanager via setProperty", () -> {
            System.setProperty("java.util.logging.manager", "Sbr3RefSysStore$MyLogManager");
            return LogManager.getLogManager().getClass().getName();
        });
        row("property reads back", () -> System.getProperty("java.util.logging.manager"));
        // 4. a property a class library reads on every call
        row("line.separator override", () -> {
            String old = System.getProperty("line.separator");
            System.setProperty("line.separator", "<NL>");
            String s = System.lineSeparator() + "|" + String.format("%n") + "|" + System.getProperty("line.separator");
            System.setProperty("line.separator", old);
            return s.replace("\n", "\\n");
        });
        row("file.encoding via Properties", () -> {
            System.getProperties().setProperty("sbr3.probe", "x");
            return System.getProperties().getProperty("sbr3.probe") + System.getProperty("sbr3.probe");
        });
        row("Integer.getInteger", () -> {
            System.setProperty("sbr3.int", "42");
            return Integer.getInteger("sbr3.int") + " " + Boolean.getBoolean("sbr3.bool") + " " + Long.getLong("sbr3.int");
        });
        row("Integer.getInteger via Properties", () -> {
            System.getProperties().put("sbr3.int2", "43");
            return Integer.getInteger("sbr3.int2");
        });
        System.out.println("PROBE-DONE");
    }
}
