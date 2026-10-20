// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 107, lane L5 -- `ClassLoader.retrieveDirectives()`
// hands the JDK's own assertion maps the command line's `-ea:<name>` /
// `-da:<name>` options (page
// docs/internal/fixed-bugs/interpreter-i106-L5-assertion-switches-are-one-global-FIXED-20261016.md,
// item 3). The first line is answered by the VM (`desiredAssertionStatus0`);
// `setPackageAssertionStatus` makes the app loader build its maps from
// `retrieveDirectives`, so the second line is answered by
// `ClassLoader.desiredAssertionStatus(String)` from those maps. Before wave
// 107 the directives were empty and `deflt` was the one global bit, so the
// second line lost every scoped option.
//
// Run:
//   javac -d out L5W107AssertionDirectivesMaps.java
//   java -cp out L5W107AssertionDirectivesMaps                      (HOTSPOT_EXPECTED below)
//   java '-ea:L5W107AssertionDirectivesMaps' '-da:L5W107AssertionDirectivesMaps$Off' \
//        -ea:java.util... -cp out L5W107AssertionDirectivesMaps
//   and the same two command lines with cratonvm --jdk-only / --compatible.
//
// Expected HotSpot 25 output with the scoped flags (derived from
// `JavaAssertions` and `ClassLoader.desiredAssertionStatus(String)`, not run):
//   vm   outer=true off=false helper=false list=true
//   maps outer=true off=false helper=false list=true
// (`list` stays a VM answer: ArrayList's loader is the boot loader.)
//
// HOTSPOT_EXPECTED_BEGIN
// vm   outer=false off=false helper=false list=false
// maps outer=false off=false helper=false list=false
// HOTSPOT_EXPECTED_END

public class L5W107AssertionDirectivesMaps {
    static final class Off {
    }

    static String row() {
        return "outer=" + L5W107AssertionDirectivesMaps.class.desiredAssertionStatus()
                + " off=" + Off.class.desiredAssertionStatus()
                + " helper=" + L5W107AssertionDirectivesMapsHelper.class.desiredAssertionStatus()
                + " list=" + java.util.ArrayList.class.desiredAssertionStatus();
    }

    public static void main(String[] args) {
        System.out.println("vm   " + row());
        ClassLoader app = L5W107AssertionDirectivesMaps.class.getClassLoader();
        app.setPackageAssertionStatus("l5w107.unrelated", false);
        System.out.println("maps " + row());
    }
}

class L5W107AssertionDirectivesMapsHelper {
}
