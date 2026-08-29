// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Definition-of-done driver: run JUnit4 classes IN PROCESS and return
// normally.
//
// `org.junit.runner.JUnitCore.main` calls System.exit, and the --jdk-only
// report is not written on that path -- the file
// simply never appears and the run reads as clean. This runs the same classes
// through `JUnitCore.run` and falls off the end of main instead.
//
// Prints only counts and failure identities the program chose. Stack traces go
// to stdout deliberately: a failure's cause is the evidence for whether the
// caller recovered from a refusal, which is the question the definition-of-done
// screen asks.

import java.util.ArrayList;
import java.util.List;

import org.junit.runner.JUnitCore;
import org.junit.runner.Result;
import org.junit.runner.notification.Failure;

public final class DodJUnitRunner {

    public static void main(String[] args) {
        int totalRun = 0;
        int totalFailed = 0;
        int totalIgnored = 0;
        List<String> broken = new ArrayList<>();

        for (String name : args) {
            Class<?> c;
            try {
                c = Class.forName(name);
            } catch (Throwable t) {
                broken.add(name);
                System.out.println("DOD CLASS-LOAD-FAILED " + name + " "
                        + t.getClass().getName() + ": " + t.getMessage());
                continue;
            }
            Result r;
            try {
                r = new JUnitCore().run(c);
            } catch (Throwable t) {
                broken.add(name);
                System.out.println("DOD RUN-THREW " + name + " "
                        + t.getClass().getName() + ": " + t.getMessage());
                t.printStackTrace(System.out);
                continue;
            }
            totalRun += r.getRunCount();
            totalFailed += r.getFailureCount();
            totalIgnored += r.getIgnoreCount();
            System.out.println("DOD CLASS " + name
                    + " run=" + r.getRunCount()
                    + " failed=" + r.getFailureCount()
                    + " ignored=" + r.getIgnoreCount());
            for (Failure f : r.getFailures()) {
                System.out.println("DOD FAILURE " + f.getTestHeader() + " :: "
                        + (f.getException() == null ? "?" : f.getException().getClass().getName())
                        + ": " + f.getMessage());
                if (f.getException() != null) {
                    f.getException().printStackTrace(System.out);
                }
            }
        }

        System.out.println("DOD TOTAL run=" + totalRun + " failed=" + totalFailed
                + " ignored=" + totalIgnored + " broken=" + broken.size());
        System.out.println("DOD RESULT "
                + (totalFailed == 0 && broken.isEmpty() ? "OK" : "FAILURES"));
    }
}
