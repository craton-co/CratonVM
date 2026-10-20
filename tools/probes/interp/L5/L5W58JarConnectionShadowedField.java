// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- `JarURLConnection.getEntryName()`
// / `getJarFileURL()` answer the fields `java.net.JarURLConnection` declares,
// even when a subclass declares its OWN fields of the same names. Spring Boot's
// `JarUrlConnection` does exactly that (`private final String entryName`,
// assigned from `getEntryName()` in its constructor); wave 57's natives read
// the field by name, found the subclass's still-null one, and every
// nested-jar entry read as its jar's root: the Spring Boot fat jar's main
// class was defined from the nested archive's bytes (`ClassFormatError:
// Incompatible magic value 1347093252`)
// (`docs/internal/fixed-bugs/interpreter-L5-a-windows-fat-jar-reads-an-entry-at-the-wrong-offset-FIXED-20261022.md`).
//
// Run (no setup):
//   javac -d out L5W58JarConnectionShadowedField.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W58JarConnectionShadowedField
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   ctor-1=entry=a/b.txt url=file:/x/y.jar
//   later-1=entry=a/b.txt url=file:/x/y.jar
//   ctor-2=entry=a/b.txt url=file:/x/y.jar
//   later-2=entry=a/b.txt url=file:/x/y.jar
//
// CratonVM in wave 57 (both modes): the natives read the receiver's field by
// name, which is this subclass's (traced on the Spring Boot fat jar: the
// `entryName` read answered null while `JarUrlConnection` was being
// constructed), so `entry=null url=null` on the ctor rows and, the subclass's
// fields then being those nulls, on the later rows too.

import java.net.JarURLConnection;
import java.net.URL;
import java.util.jar.JarFile;

public class L5W58JarConnectionShadowedField {
    static class Shadowing extends JarURLConnection {
        private final String entryName;
        private final URL jarFileURL;

        Shadowing(URL u) throws Exception {
            super(u);
            this.entryName = getEntryName();
            this.jarFileURL = getJarFileURL();
        }

        String seen() {
            return "entry=" + entryName + " url=" + jarFileURL;
        }

        @Override
        public JarFile getJarFile() {
            return null;
        }

        @Override
        public void connect() {
        }
    }

    public static void main(String[] args) throws Exception {
        URL u = new URL("jar:file:/x/y.jar!/a/b.txt");
        for (String tag : new String[] {"1", "2"}) {
            Shadowing c = new Shadowing(u);
            System.out.println("ctor-" + tag + "=" + c.seen());
            System.out.println("later-" + tag + "=entry=" + c.getEntryName() + " url=" + c.getJarFileURL());
        }
    }
}
