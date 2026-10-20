// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Properties;

/**
 * SBR round 4, lane {@code lang}: do the readers of the system-property store agree with the
 * real {@code System.props} after each kind of write? One line per case; diff against HotSpot.
 * Control: {@code CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/System.setProperty,java/lang/System.clearProperty,java/lang/System.getProperties}
 *
 * <pre>cratonvm --jdk-only -cp . Sbr4LangPropStore</pre>
 */
public final class Sbr4LangPropStore {
    static void line(String name, Object v) {
        System.out.println(name + " = " + v);
    }

    public static void main(String[] a) throws Exception {
        System.setProperty("sbr4.k1", "v1");
        line("getProperty after setProperty", System.getProperty("sbr4.k1"));
        line("getProperty default after setProperty", System.getProperty("sbr4.k1", "dflt"));
        line("Boolean.getBoolean", Boolean.getBoolean("sbr4.k1"));
        System.setProperty("sbr4.int", "42");
        line("Integer.getInteger", Integer.getInteger("sbr4.int"));
        Properties p = System.getProperties();
        p.setProperty("sbr4.k2", "held");
        line("getProperty after held-Properties set", System.getProperty("sbr4.k2"));
        p.put("sbr4.k3", "put");
        line("getProperty after held-Properties put", System.getProperty("sbr4.k3"));
        System.clearProperty("sbr4.k1");
        line("getProperty after clearProperty", System.getProperty("sbr4.k1"));
        p.remove("sbr4.k2");
        line("getProperty after held-Properties remove", System.getProperty("sbr4.k2"));
        line("getProperty remove default", System.getProperty("sbr4.k2", "gone"));
        System.setProperty("user.language", "xx");
        line("user.language", System.getProperty("user.language"));
        System.setProperty("line.separator", "|");
        line("lineSeparator", System.lineSeparator().length());
        System.setProperty("java.io.tmpdir", "/sbr4tmp");
        line("tmpdir", System.getProperty("java.io.tmpdir"));
        System.setProperty("file.encoding", "ISO-8859-1");
        line("file.encoding", System.getProperty("file.encoding"));
        System.setProperty("user.dir", "/sbr4dir");
        line("user.dir", System.getProperty("user.dir"));
        line("new File(\"x\").getAbsolutePath", new java.io.File("x").getAbsolutePath().endsWith("/x"));
        System.setProperty("java.class.path", "/sbr4cp");
        line("java.class.path", System.getProperty("java.class.path"));
        System.setProperty("java.library.path", "/sbr4lib");
        try { System.loadLibrary("sbr4nope"); } catch (UnsatisfiedLinkError e) { line("loadLibrary", e.getMessage()); }
        System.setProperty("java.net.preferIPv4Stack", "true");
        line("InetAddress.getLoopbackAddress", java.net.InetAddress.getLoopbackAddress());
        System.setProperty("http.proxyHost", "proxy.sbr4");
        System.setProperty("http.proxyPort", "8080");
        line("ProxySelector", java.net.ProxySelector.getDefault().select(new java.net.URI("http://example.com/")));
        line("size grows", System.getProperties().containsKey("sbr4.k3"));
        System.setProperty("sbr4.k4", "x");
        line("stringPropertyNames has k4", System.getProperties().stringPropertyNames().contains("sbr4.k4"));
        System.setProperties(null);
        line("after setProperties(null) k3", System.getProperty("sbr4.k3"));
        line("after setProperties(null) java.version set", System.getProperty("java.version") != null);
    }
}
