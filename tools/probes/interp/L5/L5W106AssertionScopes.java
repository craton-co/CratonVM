// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- `Class.desiredAssertionStatus()`
// under the assertion switches (page
// docs/internal/fixed-bugs/interpreter-i106-L5-assertion-switches-are-one-global-FIXED-20261016.md).
// HotSpot keeps a user default (-ea/-da), a system default (-esa/-dsa, the
// boot loader's classes) and per-class / per-package lists
// (`JavaAssertions::enabled`); CratonVM folds the unscoped switches into one
// global bit and ignores the scoped ones. `user` and `inner` are app-loader
// classes, `string` and `list` are boot classes.
//
// Run (no setup; one run per flag set, both modes):
//   javac -d out L5W106AssertionScopes.java
//   java     <flags> -cp out L5W106AssertionScopes
//   cratonvm --jdk-only <flags> -cp out L5W106AssertionScopes   (and --compatible)
//   e.g. (bash) for f in "" "-ea" "-esa" "-ea -da:L5W106AssertionScopes"
//     '-ea -da:L5W106AssertionScopes$Inner' "-ea:..." "-ea:L5W106AssertionScopes"
//     "-ea:java.util..." "-da -esa"   (one line); do echo "[$f]";
//     java $f -cp out L5W106AssertionScopes; done
//
// Expected HotSpot 25 output per <flags> (M = measured with HotSpot 25 on
// Windows w106; D = derived from `JavaAssertions::enabled`, not run):
//   (none)                                 M user=false inner=false string=false list=false assert-fired=false
//   -ea                                    M user=true inner=true string=false list=false assert-fired=true
//   -esa                                   M user=false inner=false string=true list=true assert-fired=false
//   -ea -da:L5W106AssertionScopes          M user=false inner=true string=false list=false assert-fired=false
//   -ea -da:L5W106AssertionScopes$Inner    D user=true inner=false string=false list=false assert-fired=true
//   -ea:...                                D user=true inner=true string=false list=false assert-fired=true
//   -ea:L5W106AssertionScopes              D user=true inner=false string=false list=false assert-fired=true
//   -ea:java.util...                       M user=false inner=false string=false list=true assert-fired=false
//   -da -esa                               M user=false inner=false string=true list=true assert-fired=false
// (`assert-fired` follows `user`: javac arms a nested class's asserts from
// the OUTERMOST class's desiredAssertionStatus, and `main` is in the outer.)
// CratonVM before wave 107 (one global bit, measured on w106, both modes):
// -ea answered string=true list=true; -esa, -da -esa and -ea -da:<outer>
// answered all true; -ea:java.util... answered list=false.
// Wave 107 (lane L5): per-VM `JavaAssertions` directives
// (`VmConfig::assertion_directives`); every row above should now match.
//
// HOTSPOT_EXPECTED_BEGIN
// user=false inner=false string=false list=false assert-fired=false
// HOTSPOT_EXPECTED_END

public class L5W106AssertionScopes {
    static final class Inner {
    }

    public static void main(String[] args) {
        boolean fired = false;
        try {
            assert false;
        } catch (AssertionError e) {
            fired = true;
        }
        System.out.println("user=" + L5W106AssertionScopes.class.desiredAssertionStatus()
                + " inner=" + Inner.class.desiredAssertionStatus()
                + " string=" + String.class.desiredAssertionStatus()
                + " list=" + java.util.ArrayList.class.desiredAssertionStatus()
                + " assert-fired=" + fired);
    }
}
