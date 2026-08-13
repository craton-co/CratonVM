// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Does the object a List's `listIterator()` hands back actually IMPLEMENT
// `java.util.ListIterator`?
//
// The contract is trivial and universal: `List.listIterator()` is declared to
// return `ListIterator<E>`, so its product is `instanceof ListIterator` and
// `instanceof Iterator` on every conformant JVM. There is no configuration,
// no mode and no image in which that can be false.
//
// ---------------------------------------------------------------------------
// Why this file exists
// ---------------------------------------------------------------------------
//
// CratonVM serves `java.util.LinkedList.listIterator()` from a native that
// returns an instance of an internal carrier class,
// `cratonvm/internal/LinkedListSnapshotListItr`. Interfaces for a minted class
// come from one table — `jdk_interfaces` in `classloading/src/class_manager.rs`
// — and the carrier had no arm in it, so it fell to that match's `_ => &[]`
// default and implemented NOTHING.
//
// The failure is invisible on the path the carrier was built for.
// `AbstractList.equals` / `hashCode` / `indexOf` obtain it through a variable
// already typed `ListIterator`, so javac emits no `checkcast` and the family
// works. It is user code that goes through `Object`, or through any erased
// generic, that meets the defect:
//
//     ClassCastException: cratonvm.internal.LinkedListSnapshotListItr
//                         cannot be cast to java.util.ListIterator
//
// Until 2026-08-11 this was a `--real-jdk` defect only, because `--jdk-only`
// refused to mint the carrier at all and raised `NoClassDefFoundError` earlier.
// Commit 6ae3ca634 landed the mint (through the VM-internal door) and the
// registration retag, without the `jdk_interfaces` arm that was recorded
// alongside them — so the residual got WORSE as a side effect of its own fix
// landing: the ClassCastException became reachable in BOTH modes.
// W7-16-arraydeque-and-linkedlist-residuals.md
// W7-20-refusal-laundered-into-wrong-answer.md
// W7-62-ratchets-and-dead-code.md
//
// ---------------------------------------------------------------------------
// The two ways a probe like this lies, and what is done about each
// ---------------------------------------------------------------------------
//
// 1. THE CAST IS NOT ACTUALLY A `checkcast`. If the reference were held in a
//    variable already typed `ListIterator`, javac would emit no checkcast and
//    every row would pass on a VM where the class implements nothing —
//    reporting green for the exact defect it exists to find. Every cast row
//    therefore launders the reference through a `static Object opaque(Object)`
//    that javac cannot see through, so the checkcast is unavoidable. The
//    `selfTestNoCheckcast` row is the calibration: it does the SAME cast on a
//    reference javac already knows the type of, and asserts it succeeds. If
//    that row ever failed, no other row in the file would mean anything.
//
// 2. IT PASSES BECAUSE THE WHOLE CLASS IS FINE, NOT BECAUSE THE ROW IS. Every
//    row runs against BOTH `ArrayList` and `LinkedList`. `ArrayList` is the
//    CONTROL: CratonVM already carries a `jdk_interfaces` arm for
//    `java/util/ArrayList$ListItr`, so a run in which the ArrayList rows fail
//    is a broken instrument, not a finding.
//
// ---------------------------------------------------------------------------
// Reading the output
// ---------------------------------------------------------------------------
//
//   ROW <name> <observed> want=<expected> <PASS|FAIL>
//
// `getClass` and `interfaces` rows are INFORMATIONAL (`want=-`) and never
// scored. `listIterator().getClass()` legitimately differs from HotSpot on
// CratonVM — the carrier is deliberately not named `java.util.LinkedList$ListItr`,
// because that name resolves to the real 5-field class whose layout mangles the
// carrier's Int cursor. That row is printed to make the difference legible, not
// to fail on it.
//
// ---------------------------------------------------------------------------
// Expected before and after the `jdk_interfaces` arm
// ---------------------------------------------------------------------------
//
//   arm             HotSpot 25   CratonVM --real-jdk   CratonVM --jdk-only
//   ll.instanceof   true         BEFORE false          BEFORE false
//   ll.cast         ok           BEFORE CCE            BEFORE CCE
//   ll.iterCast     ok           BEFORE CCE            BEFORE CCE
//                                AFTER  all three rows as HotSpot, both modes
//
// The `al.*` control rows are expected green in every arm, before and after.
// If an `al.*` row is red the run says nothing about the `ll.*` rows.

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Iterator;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;

public final class ListItrInterfaceProbe {

    private static int pass = 0;
    private static int fail = 0;

    /** Launders a reference so javac cannot prove its type and MUST emit a
     *  checkcast at the use site. Without this the whole probe is vacuous. */
    private static Object opaque(Object o) {
        return o;
    }

    private static void row(String name, String observed, String want) {
        boolean ok = want.equals("-") || want.equals(observed);
        if (!want.equals("-")) {
            if (ok) {
                pass++;
            } else {
                fail++;
            }
        }
        System.out.println(
            "ROW " + name + " " + observed + " want=" + want + " "
            + (want.equals("-") ? "INFO" : (ok ? "PASS" : "FAIL")));
    }

    /** `(ListIterator) o` where `o` is statically `Object`. */
    private static String castToListIterator(Object o) {
        try {
            ListIterator<?> li = (ListIterator<?>) o;
            return "ok:" + (li != null);
        } catch (ClassCastException e) {
            return "ClassCastException";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }

    /** `(Iterator) o` where `o` is statically `Object`. `ListIterator extends
     *  Iterator`, so a VM that gets one right and the other wrong has a
     *  transitivity bug rather than a missing-arm bug — worth telling apart. */
    private static String castToIterator(Object o) {
        try {
            Iterator<?> it = (Iterator<?>) o;
            return "ok:" + (it != null);
        } catch (ClassCastException e) {
            return "ClassCastException";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }

    private static String instanceOfListIterator(Object o) {
        return String.valueOf(o instanceof ListIterator);
    }

    private static String instanceOfIterator(Object o) {
        return String.valueOf(o instanceof Iterator);
    }

    private static String interfacesOf(Object o) {
        Class<?>[] ifs = o.getClass().getInterfaces();
        String[] names = new String[ifs.length];
        for (int i = 0; i < ifs.length; i++) {
            names[i] = ifs[i].getName();
        }
        Arrays.sort(names);
        return names.length == 0 ? "[]" : Arrays.toString(names);
    }

    private static void family(String tag, List<String> list) {
        Object li = opaque(list.listIterator());
        row(tag + ".getClass", li.getClass().getName(), "-");
        row(tag + ".interfaces", interfacesOf(li), "-");
        row(tag + ".instanceofListIterator", instanceOfListIterator(li), "true");
        row(tag + ".instanceofIterator", instanceOfIterator(li), "true");
        row(tag + ".castListIterator", castToListIterator(li), "ok:true");
        row(tag + ".castIterator", castToIterator(li), "ok:true");

        // `listIterator(int)` is a second native on CratonVM and mints the same
        // carrier; a fix applied to one and not the other is a real shape.
        Object li2 = opaque(list.listIterator(0));
        row(tag + ".idx.castListIterator", castToListIterator(li2), "ok:true");

        // `iterator()` is a DIFFERENT carrier on CratonVM
        // (`java/util/LinkedList$Itr`, which already has an arm). Printed so a
        // reader can see the two are not the same object model.
        Object it = opaque(list.iterator());
        row(tag + ".iterator.getClass", it.getClass().getName(), "-");
        row(tag + ".iterator.castIterator", castToIterator(it), "ok:true");
    }

    public static void main(String[] args) {
        System.out.println("PROBE java=" + System.getProperty("java.version")
            + " vm=" + System.getProperty("java.vm.name"));

        // CALIBRATION. The same cast on a reference javac already knows the
        // type of, so no checkcast is emitted. This row passing proves nothing
        // about the VM; this row FAILING proves the instrument is broken.
        ListIterator<String> known = new ArrayList<String>(List.of("a")).listIterator();
        ListIterator<?> stillKnown = (ListIterator<?>) known;
        row("selfTestNoCheckcast", "ok:" + (stillKnown != null), "ok:true");

        // THE RED, CALIBRATED. Everything below reports PASS when a cast
        // SUCCEEDS, so a helper that swallowed the ClassCastException — or a
        // checkcast the VM elided — would turn every row green while proving
        // nothing. This row is the only one whose expected answer is the
        // exception, and it must FAIL if `castToListIterator` ever stops being
        // able to report one. A green run with this row missing or passing
        // vacuously is not evidence.
        row("selfTestRed", castToListIterator(opaque(new Object())), "ClassCastException");
        row("selfTestRedIterator", castToIterator(opaque(new Object())), "ClassCastException");
        row("selfTestRedInstanceof", instanceOfListIterator(opaque(new Object())), "false");

        family("al", new ArrayList<>(List.of("a", "b", "c")));
        family("ll", new LinkedList<>(List.of("a", "b", "c")));

        System.out.println("SUMMARY pass=" + pass + " fail=" + fail);
        System.exit(fail == 0 ? 0 : 1);
    }
}
