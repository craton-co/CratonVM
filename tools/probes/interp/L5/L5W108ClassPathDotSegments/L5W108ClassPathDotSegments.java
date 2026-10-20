// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 108, lane L5 -- see setup.sh for the build, the
// run commands and the expected output. main.jar's `Class-Path: dep.jar
// sub/../dep.jar ./dep.jar` names ONE jar three ways. JDK 25's
// `new URL(base, token)` folds `.` and `..` segments (`URLStreamHandler.
// parseURL`) and `URLClassPath` keys its loaders by that URL, so dep.jar is
// one loader; CratonVM kept three entries (paths compared as spelled).

import java.net.URL;
import java.util.Collections;
import java.util.List;

public class L5W108ClassPathDotSegments {
    static String jarOf(URL u) {
        String s = u.toString();
        int bang = s.indexOf("!/");
        String jar = bang < 0 ? s : s.substring(0, bang);
        return jar.substring(jar.lastIndexOf('/') + 1);
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W108ClassPathDotSegments.class.getClassLoader();
        List<URL> urls = Collections.list(app.getResources("dep/d.txt"));
        System.out.println("getResources count=" + urls.size());
        for (int i = 0; i < urls.size(); i++) {
            String s = urls.get(i).toString();
            System.out.println("url" + i + " dot-segments=" + (s.contains("/../") || s.contains("/./"))
                    + " jar=" + jarOf(urls.get(i)));
        }
        URL one = app.getResource("dep/d.txt");
        System.out.println("getResource dot-segments="
                + (one != null && (one.toString().contains("/../") || one.toString().contains("/./"))));
        Class<?> c = Class.forName("dep.D", false, app);
        URL src = c.getProtectionDomain().getCodeSource().getLocation();
        System.out.println("dep.D source dot-segments="
                + (src.toString().contains("/../") || src.toString().contains("/./"))
                + " jar=" + jarOf(src));
    }
}
