// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- the application loader finds a
// class file under the requested name whose `this_class` is another class:
// HotSpot's `defineClass` fails with `NoClassDefFoundError: <requested>
// (wrong name: <declared>)`, and `Class.forName` lets it through (it is the
// launcher's `Caused by:` for `java -cp out/pkg Main` too). The first row is
// repeated last (cold first call).
//
// SETUP and run (the package directory is a second class-path entry):
//   mkdir -p src/l5w59pkg
//   printf 'package l5w59pkg; public class Target {}\n' > src/l5w59pkg/Target.java
//   javac -d out L5W59AppLoaderWrongName.java src/l5w59pkg/Target.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out:out/l5w59pkg L5W59AppLoaderWrongName
// (`;` instead of `:` on Windows.)
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   forname=java.lang.NoClassDefFoundError: Target (wrong name: l5w59pkg/Target)
//   loadclass=java.lang.NoClassDefFoundError: Target (wrong name: l5w59pkg/Target)
//   right-name=l5w59pkg.Target
//   forname-again=java.lang.NoClassDefFoundError: Target (wrong name: l5w59pkg/Target)
//
// CratonVM (debug build, Windows, wave 59, both modes; the launcher shows the
// same as `java.lang.ClassNotFoundException: Main`): see
// `docs/internal/fixed-bugs/interpreter-L5-the-application-loader-answers-a-wrong-name-class-file-with-classnotfound-FIXED-20261024.md`
// (fixed by wave 60; `--jdk-only` was already HotSpot's on the base).

import java.util.concurrent.Callable;

public class L5W59AppLoaderWrongName {
    static String attempt(Callable<Object> c) {
        try {
            Object r = c.call();
            return r instanceof Class<?> k ? k.getName() : String.valueOf(r);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("forname=" + attempt(() -> Class.forName("Target", false, app)));
        System.out.println("loadclass=" + attempt(() -> app.loadClass("Target")));
        System.out.println("right-name=" + attempt(() -> Class.forName("l5w59pkg.Target", false, app)));
        System.out.println("forname-again=" + attempt(() -> Class.forName("Target", false, app)));
    }
}
