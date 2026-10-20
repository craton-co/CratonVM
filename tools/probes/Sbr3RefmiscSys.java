// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Properties;
import java.util.function.Supplier;

/**
 * SBR round 3, lane {@code ref}: {@code System.getProperties / setProperty / clearProperty /
 * exit}. A write through the {@code Properties} object must be visible to everything that reads a
 * property natively (tmpdir, user.dir, file.encoding consumers), and a native-side change must be
 * visible through the object. {@code exit} runs the shutdown hooks and returns the status.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefmiscSys [exit]    # exit code 7 with a hook line
 * </pre>
 */
public final class Sbr3RefmiscSys {
    static void row(String name, Supplier<Object> s) {
        try {
            System.out.println(name + " = " + s.get());
        } catch (Throwable t) {
            System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("exit")) {
            Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook ran")));
            System.out.println("before exit");
            System.out.flush();
            System.exit(7);
            System.out.println("NOT REACHED");
        }
        row("setProperty new", () -> System.setProperty("sbr3.k1", "v1"));
        row("getProperty", () -> System.getProperty("sbr3.k1"));
        row("setProperty old", () -> System.setProperty("sbr3.k1", "v2"));
        row("props sees", () -> System.getProperties().getProperty("sbr3.k1"));
        row("props put visible to getProperty", () -> {
            System.getProperties().setProperty("sbr3.k2", "p2");
            return System.getProperty("sbr3.k2");
        });
        row("clearProperty", () -> System.clearProperty("sbr3.k1"));
        row("clearProperty missing", () -> System.clearProperty("sbr3.nope"));
        row("getProperty after clear", () -> System.getProperty("sbr3.k1"));
        row("props remove visible", () -> {
            System.getProperties().remove("sbr3.k2");
            return System.getProperty("sbr3.k2");
        });
        row("setProperty null key", () -> System.setProperty(null, "x"));
        row("setProperty empty key", () -> System.setProperty("", "x"));
        row("setProperty null value", () -> System.setProperty("sbr3.k3", null));
        row("clearProperty null", () -> System.clearProperty(null));
        row("clearProperty empty", () -> System.clearProperty(""));
        row("getProperties same object", () -> System.getProperties() == System.getProperties());
        row("props class", () -> System.getProperties().getClass().getName());
        row("java.version set", () -> System.getProperty("java.version") != null && System.getProperty("java.specification.version").equals("25"));
        row("line.separator", () -> System.getProperty("line.separator").length());
        row("file.separator", () -> System.getProperty("file.separator"));
        row("jboss.home.dir", () -> System.getProperty("jboss.home.dir"));
        row("reactor.netty.ioWorkerCount", () -> System.getProperty("reactor.netty.ioWorkerCount"));

        // natives read these: tmpdir through File.createTempFile, user.dir through File.getAbsolutePath
        Path tmp = Files.createTempDirectory("sbr3ref");
        String oldTmp = System.getProperty("java.io.tmpdir");
        System.setProperty("java.io.tmpdir", tmp.toString());
        row("tmpdir effect", () -> {
            try {
                File f = File.createTempFile("sbr3", ".t");
                boolean inside = f.getParentFile().toPath().toRealPath().equals(tmp.toRealPath());
                f.delete();
                return inside;
            } catch (Exception e) {
                return e.toString();
            }
        });
        System.getProperties().setProperty("java.io.tmpdir", oldTmp);
        row("tmpdir restored via props", () -> {
            try {
                File f = File.createTempFile("sbr3", ".t");
                boolean inside = f.getParentFile().toPath().toRealPath().equals(Path.of(oldTmp).toRealPath());
                f.delete();
                return inside;
            } catch (Exception e) {
                return e.toString();
            }
        });
        String oldDir = System.getProperty("user.dir");
        row("user.dir set", () -> {
            System.setProperty("user.dir", tmp.toString());
            return new File("rel").getAbsolutePath().startsWith(tmp.toString());
        });
        System.getProperties().setProperty("user.dir", oldDir);
        row("user.dir restored via props", () -> new File("rel").getAbsolutePath().startsWith(oldDir));
        row("file.encoding observable", () -> System.getProperty("file.encoding"));

        // setProperties swaps the object
        Properties original = System.getProperties();
        Properties mine = new Properties();
        mine.putAll(original);
        mine.setProperty("sbr3.swapped", "yes");
        row("setProperties", () -> {
            System.setProperties(mine);
            return System.getProperty("sbr3.swapped") + " " + (System.getProperties() == mine);
        });
        System.setProperties(original);
        row("setProperties restored", () -> System.getProperty("sbr3.swapped") + " " + (System.getProperties() == original));
        row("setProperties null", () -> {
            Properties prev = System.getProperties();
            System.setProperties(null);
            Properties now = System.getProperties();
            boolean fresh = now != prev;
            System.setProperties(prev);
            return fresh + " " + (now.getProperty("java.version") != null);
        });

        // concurrency: many writers through both doors
        Thread[] ts = new Thread[4];
        for (int i = 0; i < ts.length; i++) {
            final int id = i;
            ts[i] = new Thread(() -> {
                for (int j = 0; j < 300; j++) {
                    System.setProperty("sbr3.t" + id, "" + j);
                    System.getProperties().setProperty("sbr3.u" + id, "" + j);
                    System.clearProperty("sbr3.t" + id);
                }
            });
            ts[i].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        row("threads end", () -> System.getProperty("sbr3.t0") + " " + System.getProperty("sbr3.u3"));
        System.out.println("PROBE-DONE");
    }
}
