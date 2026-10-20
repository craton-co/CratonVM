// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- `-XX:DiagnoseSyncOnValueBasedClasses`
// (`VmConfig::diagnose_sync_on_value_based_classes`; page
// `i101-L6-diagnose-sync-on-value-based-classes-ignored`).
//
// HotSpot 25 (`ObjectSynchronizer::handle_sync_on_value_based_class`), at the
// `monitorenter` on an `Integer` (a `@jdk.internal.ValueBased` boot class):
//   =0 (default): nothing; the lock works.
//   =1: fatal error, stdout header with
//       `#  fatal error: Synchronizing on object 0x<addr> of klass java.lang.Integer at L6W102DiagnoseSyncValueBased.main(L6W102DiagnoseSyncValueBased.java:<line>)`
//       then abort (no "after" line; exit status 134 on Linux).
//   =2: on stdout (the tag is configured to stdout at info by this mode)
//       [<uptime>][info][valuebasedclasses] Synchronizing on object 0x<addr> of klass java.lang.Integer
//       [<uptime>][info][valuebasedclasses] \tat L6W102DiagnoseSyncValueBased.main(L6W102DiagnoseSyncValueBased.java:<line>)
//       [<uptime>][info][valuebasedclasses] \t- locked <0x<addr>> (a java.lang.Integer)
//       then the run continues. A plain `Object` lock prints nothing.
// CratonVM ignored the flag (the launcher dropped it as an unknown -XX).
//
// Run (compare with the address and uptime masked):
//   java -cp out L6W102DiagnoseSyncValueBased
//   java -XX:+UnlockDiagnosticVMOptions -XX:DiagnoseSyncOnValueBasedClasses=2 -cp out L6W102DiagnoseSyncValueBased
//   java -XX:+UnlockDiagnosticVMOptions -XX:DiagnoseSyncOnValueBasedClasses=1 -cp out L6W102DiagnoseSyncValueBased
//   java -XX:+UnlockDiagnosticVMOptions -XX:DiagnoseSyncOnValueBasedClasses=3 -cp out L6W102DiagnoseSyncValueBased
//     (stderr: `int DiagnoseSyncOnValueBasedClasses=3 is outside the allowed range [ 0 ... 2 ]`,
//      `Improperly specified VM option 'DiagnoseSyncOnValueBasedClasses=3'`, the two
//      `Error:` lines, exit 1)
// Mask: sed -E 's/0x[0-9a-f]{16}/0xADDR/g; s/^\[[0-9.]+s\]/[UP]/'
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// before
// plain object locked
// value-based locked
// after
// HOTSPOT_EXPECTED_END

public class L6W102DiagnoseSyncValueBased {
    static final Object PLAIN = new Object();

    public static void main(String[] args) {
        Integer boxed = Integer.valueOf(4242);
        System.out.println("before");
        synchronized (PLAIN) {
            System.out.println("plain object locked");
        }
        synchronized (boxed) {
            System.out.println("value-based locked");
        }
        System.out.println("after");
    }
}
