// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- see setup.sh for the build, the
// run commands and the expected output. `dep.D` and `dep/d.txt` live only in
// `lib dir/dep.jar`, reached through main.jar's `Class-Path: lib%20dir/dep.jar`.

import java.io.InputStream;

public class L5W106ClassPathPercentEncoded {
    public static void main(String[] args) throws Exception {
        String cls;
        try {
            Class<?> d = Class.forName("dep.D");
            ClassLoader l = d.getClassLoader();
            cls = "class=" + d.getName() + " says=" + d.getMethod("says").invoke(null)
                    + " loader=" + (l == ClassLoader.getSystemClassLoader() ? "app" : String.valueOf(l));
        } catch (Throwable t) {
            cls = "class=" + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(cls);
        String res;
        try (InputStream in = L5W106ClassPathPercentEncoded.class.getClassLoader()
                .getResourceAsStream("dep/d.txt")) {
            res = in == null ? "null" : new String(in.readAllBytes(), "UTF-8");
        }
        System.out.println("resource=" + res);
    }
}
