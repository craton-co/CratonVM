// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 109, lane L5 -- see setup.sh for the build, the
// run commands and the expected output: one jar and one directory each
// spelled several ways on `-cp` are ONE class path element on HotSpot
// (`URLClassPath` canonicalizes `java.class.path` elements), so every
// resource of them is answered once by `getResources`.

import java.net.URL;
import java.util.Collections;
import java.util.List;

public class L5W109ClassPathSpelledTwice {
    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W109ClassPathSpelledTwice.class.getClassLoader();
        for (String name : new String[] {"a/a.txt", "d/d.txt"}) {
            List<URL> urls = Collections.list(app.getResources(name));
            System.out.println(name + " getResources count=" + urls.size());
        }
        boolean found;
        try {
            Class.forName("a.A", false, app);
            found = true;
        } catch (ClassNotFoundException e) {
            found = false;
        }
        System.out.println("a.A found=" + found);
    }
}
