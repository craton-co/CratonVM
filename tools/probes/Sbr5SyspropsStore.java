// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Properties;

/**
 * SBR round 5, lane {@code sysprops}: every kind of write to the system properties, and the
 * Rust-side readers that must see it (InetAddress, ProxySelector, Boolean/Integer.getX, File,
 * Charset, loadLibrary). One line per case; diff against HotSpot, both modes.
 * Superset of {@code Sbr4LangPropStore}.
 */
public final class Sbr5SyspropsStore {
    static void line(String name, Object v) {
        System.out.println(name + " = " + v);
    }

    public static void main(String[] a) throws Exception {
        System.setProperty("sbr5.k1", "v1");
        line("getProperty after setProperty", System.getProperty("sbr5.k1"));
        line("getProperty default after setProperty", System.getProperty("sbr5.k1", "dflt"));
        line("setProperty returns old", System.setProperty("sbr5.k1", "v1b"));
        line("setProperty returns null first", System.setProperty("sbr5.fresh", "x"));
        line("Boolean.getBoolean", Boolean.getBoolean("sbr5.k1"));
        System.setProperty("sbr5.bool", "true");
        line("Boolean.getBoolean true", Boolean.getBoolean("sbr5.bool"));
        System.setProperty("sbr5.int", "42");
        line("Integer.getInteger", Integer.getInteger("sbr5.int"));
        line("Long.getLong", Long.getLong("sbr5.int", 7L));
        Properties p = System.getProperties();
        line("getProperties identity", p == System.getProperties());
        p.setProperty("sbr5.k2", "held");
        line("getProperty after held-Properties set", System.getProperty("sbr5.k2"));
        p.put("sbr5.k3", "put");
        line("getProperty after held-Properties put", System.getProperty("sbr5.k3"));
        System.clearProperty("sbr5.k1");
        line("getProperty after clearProperty", System.getProperty("sbr5.k1"));
        line("clearProperty returns old", System.clearProperty("sbr5.k2"));
        line("clearProperty missing", System.clearProperty("sbr5.nothere"));
        p.remove("sbr5.k3");
        line("getProperty after held-Properties remove", System.getProperty("sbr5.k3"));
        line("getProperty remove default", System.getProperty("sbr5.k3", "gone"));
        System.setProperty("sbr5.k3", "back");
        line("re-set after remove", System.getProperty("sbr5.k3"));
        // VM-set keys removed by the application must stay removed
        String jv = System.getProperty("java.vm.name");
        System.clearProperty("java.vm.name");
        line("clear a boot key", System.getProperty("java.vm.name"));
        System.setProperty("java.vm.name", jv);
        line("restore a boot key", System.getProperty("java.vm.name") != null);
        System.setProperty("user.language", "xx");
        line("user.language", System.getProperty("user.language"));
        System.setProperty("line.separator", "|");
        line("lineSeparator", System.lineSeparator().length());
        System.setProperty("java.io.tmpdir", "/sbr5tmp");
        line("tmpdir", System.getProperty("java.io.tmpdir"));
        System.setProperty("file.encoding", "ISO-8859-1");
        line("file.encoding", System.getProperty("file.encoding"));
        System.setProperty("user.dir", "/sbr5dir");
        line("user.dir", System.getProperty("user.dir"));
        line("new File(\"x\").getAbsolutePath", new java.io.File("x").getAbsolutePath().endsWith("/x"));
        System.setProperty("java.class.path", "/sbr5cp");
        line("java.class.path", System.getProperty("java.class.path"));
        System.setProperty("java.library.path", "/sbr5lib");
        try { System.loadLibrary("sbr5nope"); } catch (UnsatisfiedLinkError e) { line("loadLibrary", e.getMessage().startsWith("no sbr5nope in java.library.path") && !e.getMessage().contains("sbr5lib")); }
        System.setProperty("java.net.preferIPv4Stack", "true");
        line("InetAddress.getLoopbackAddress", java.net.InetAddress.getLoopbackAddress());
        System.setProperty("http.proxyHost", "proxy.sbr5");
        System.setProperty("http.proxyPort", "8080");
        line("ProxySelector", java.net.ProxySelector.getDefault().select(new java.net.URI("http://example.com/")));
        System.clearProperty("http.proxyHost");
        System.setProperty("sbr5.k4", "x");
        line("stringPropertyNames has k4", System.getProperties().stringPropertyNames().contains("sbr5.k4"));
        line("containsKey k4", System.getProperties().containsKey("sbr5.k4"));
        // a burst of keys (forces the table to resize while readers read)
        for (int i = 0; i < 300; i++) System.setProperty("sbr5.burst." + i, "v" + i);
        int ok = 0;
        for (int i = 0; i < 300; i++) if (("v" + i).equals(System.getProperty("sbr5.burst." + i))) ok++;
        line("burst ok", ok);
        for (int i = 0; i < 300; i += 2) System.clearProperty("sbr5.burst." + i);
        int left = 0;
        for (int i = 0; i < 300; i++) if (System.getProperty("sbr5.burst." + i) != null) left++;
        line("burst left", left);
        System.setProperty("user.timezone", "Asia/Tokyo");
        line("TimeZone", java.util.TimeZone.getDefault().getID());
        Properties fresh = new Properties();
        fresh.setProperty("sbr5.fresh2", "yes");
        System.setProperties(fresh);
        line("after setProperties getProperty", System.getProperty("sbr5.fresh2"));
        line("after setProperties old key", System.getProperty("sbr5.k4"));
        line("after setProperties java.version", System.getProperty("java.version") != null);
        System.setProperty("sbr5.after", "z");
        line("set after setProperties", System.getProperty("sbr5.after"));
        System.setProperties(null);
        line("after setProperties(null) k3", System.getProperty("sbr5.k3"));
        line("after setProperties(null) java.version set", System.getProperty("java.version") != null);
        // concurrent writers and readers
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 2000; i++) {
                    System.setProperty("sbr5.c." + id + "." + (i % 50), "v" + i);
                    System.getProperty("sbr5.c." + id + "." + (i % 50));
                    Boolean.getBoolean("sbr5.c." + id + "." + (i % 50));
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        line("concurrent last", System.getProperty("sbr5.c.0.49"));
    }
}
