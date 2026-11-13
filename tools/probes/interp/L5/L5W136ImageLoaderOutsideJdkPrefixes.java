// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 136, lane L5 (page
// `i136-L5-get-class-loader-of-a-platform-image-class-outside-the-jdk-prefixes-is-null`):
// `Class.getClassLoader()` of a runtime-image class whose package is not under
// `java/`, `javax/`, `jdk/`, `sun/` or `com/sun/` but whose module the JDK's
// `ModuleLoaderMap` gives to the PLATFORM loader (`java.security.jgss`'s
// `org.ietf.jgss`, `java.xml.crypto`'s `org.jcp.xml.dsig.internal.dom`,
// `jdk.xml.dom`'s `org.w3c.dom.css`, `jdk.jsobject`'s `netscape.javascript`).
// HotSpot: the platform loader. CratonVM's `native_class_get_class_loader`
// asks the image's module map only inside its `is_jdk_pkg` prefix arm, so a
// boot-tagged class outside those prefixes answered `null`.
// Control rows: a boot-module class outside the prefixes
// (`org.w3c.dom.Node`, `java.xml`), and a `java.sql` class.
//
// Run: javac -d out L5W136ImageLoaderOutsideJdkPrefixes.java
//      cratonvm [--real-jdk|--compatible] [--nojit] -cp out L5W136ImageLoaderOutsideJdkPrefixes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3)
// org.ietf.jgss.GSSManager: platform
// org.jcp.xml.dsig.internal.dom.XMLDSigRI: platform
// org.w3c.dom.css.CSSRule: platform
// netscape.javascript.JSObject: platform
// org.w3c.dom.Node: null
// java.sql.Connection: platform
// HOTSPOT_EXPECTED_END
public class L5W136ImageLoaderOutsideJdkPrefixes {
    static String loaderOf(String name) {
        try {
            Class<?> c = Class.forName(name, false, ClassLoader.getSystemClassLoader());
            ClassLoader l = c.getClassLoader();
            if (l == null) {
                return "null";
            }
            if (l == ClassLoader.getPlatformClassLoader()) {
                return "platform";
            }
            if (l == ClassLoader.getSystemClassLoader()) {
                return "app";
            }
            return l.getClass().getName();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        for (String n : new String[] {
            "org.ietf.jgss.GSSManager",
            "org.jcp.xml.dsig.internal.dom.XMLDSigRI",
            "org.w3c.dom.css.CSSRule",
            "netscape.javascript.JSObject",
            "org.w3c.dom.Node",
            "java.sql.Connection",
        }) {
            System.out.println(n + ": " + loaderOf(n));
        }
    }
}
