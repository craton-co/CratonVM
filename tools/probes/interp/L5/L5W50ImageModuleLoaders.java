// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 50, lane L5 -- the loader and CodeSource of
// runtime-image classes outside the boot loader's modules
// (docs/internal/fixed-bugs/interpreter-L5-runtime-image-module-classes-answer-a-synthetic-code-source-FIXED-20261014.md).
//
// The JDK's ModuleLoaderMap gives `java.sql` to the PLATFORM loader and
// `jdk.compiler` to the APPLICATION loader; their classes' CodeSource is
// `jrt:/<module>`. This VM defines every image class in its boot namespace:
// before wave 50 `java.sql.Connection` answered the synthesised
// `file:/runtime-defined/java/sql/Connection.class` and
// `com.sun.tools.javac.api.JavacTool` reported the boot loader (null) and no
// CodeSource, in both modes. `CRATONVM_IMAGE_APP_MODULE_LOADERS=0` restores
// that (positive control). Each row runs twice (first and again).
//
// Run (no setup):
//   javac -d out L5W50ImageModuleLoaders.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W50ImageModuleLoaders
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   1 java.lang.String loader=boot cs=null
//   1 java.sql.Connection loader=platform cs=jrt:/java.sql
//   1 com.sun.tools.javac.api.JavacTool loader=app cs=jrt:/jdk.compiler
//   1 javax.tools.ToolProvider loader=platform cs=jrt:/java.compiler
//   2 java.lang.String loader=boot cs=null
//   2 java.sql.Connection loader=platform cs=jrt:/java.sql
//   2 com.sun.tools.javac.api.JavacTool loader=app cs=jrt:/jdk.compiler
//   2 javax.tools.ToolProvider loader=platform cs=jrt:/java.compiler

import java.security.CodeSource;
import java.security.ProtectionDomain;

public class L5W50ImageModuleLoaders {
    static String loaderName(ClassLoader l) {
        if (l == null) {
            return "boot";
        }
        if (l == ClassLoader.getPlatformClassLoader()) {
            return "platform";
        }
        if (l == ClassLoader.getSystemClassLoader()) {
            return "app";
        }
        return l.getClass().getName();
    }

    static void row(int pass, String name) {
        try {
            Class<?> c = Class.forName(name, false, ClassLoader.getSystemClassLoader());
            ProtectionDomain pd = c.getProtectionDomain();
            CodeSource cs = pd == null ? null : pd.getCodeSource();
            System.out.println(pass + " " + name + " loader=" + loaderName(c.getClassLoader())
                    + " cs=" + (cs == null ? "null" : String.valueOf(cs.getLocation())));
        } catch (Throwable t) {
            System.out.println(pass + " " + name + " threw " + t);
        }
    }

    public static void main(String[] args) {
        String[] names = {"java.lang.String", "java.sql.Connection",
            "com.sun.tools.javac.api.JavacTool", "javax.tools.ToolProvider"};
        for (int pass = 1; pass <= 2; pass++) {
            for (String n : names) {
                row(pass, n);
            }
        }
    }
}
