#!/usr/bin/env python3
"""Generate RJdkVarHandleNullCoord.java, the vector beside this file.

Checked in so the vector can be REGENERATED rather than hand-edited: a
row is a signature-polymorphic call site, and hand-editing one is how a
stray cast silently tests a different access mode.

Written rather than typed because the matrix is ~90 signature-polymorphic call
sites and each one needs its own exact static types — a hand-typed table is
where a wrong cast silently changes which access mode is being tested.
"""

READ = ["get", "getVolatile", "getOpaque", "getAcquire"]
WRITE = ["set", "setVolatile", "setOpaque", "setRelease"]
CAS = ["compareAndSet", "weakCompareAndSet", "weakCompareAndSetPlain",
       "weakCompareAndSetAcquire", "weakCompareAndSetRelease"]
CAE = ["compareAndExchange", "compareAndExchangeAcquire", "compareAndExchangeRelease"]
GAS = ["getAndSet", "getAndSetAcquire", "getAndSetRelease"]
GAA = ["getAndAdd", "getAndAddAcquire", "getAndAddRelease"]
BITS = [f"getAndBitwise{op}{suf}"
        for op in ("Or", "And", "Xor") for suf in ("", "Acquire", "Release")]

def case(label, expr):
    return (f'        probe("{label}", () -> {{ {expr} }});\n')

lines = []

# ---- instance field over `int`, null receiver -----------------------------
for m in READ:
    lines.append(case(f"int-instance.{m}", f'return (int) VH_I.{m}((H) null);'))
for m in WRITE:
    lines.append(case(f"int-instance.{m}", f'VH_I.{m}((H) null, 7); return VOID;'))
for m in CAS:
    lines.append(case(f"int-instance.{m}", f'return VH_I.{m}((H) null, 0, 7);'))
for m in CAE:
    lines.append(case(f"int-instance.{m}", f'return (int) VH_I.{m}((H) null, 0, 7);'))
for m in GAS + GAA:
    lines.append(case(f"int-instance.{m}", f'return (int) VH_I.{m}((H) null, 7);'))
for m in BITS:
    lines.append(case(f"int-instance.{m}", f'return (int) VH_I.{m}((H) null, 7);'))

# ---- instance field over `Object`, null receiver --------------------------
for m in READ:
    lines.append(case(f"ref-instance.{m}", f'return VH_O.{m}((H) null);'))
for m in WRITE:
    lines.append(case(f"ref-instance.{m}", f'VH_O.{m}((H) null, "x"); return VOID;'))
for m in CAS:
    lines.append(case(f"ref-instance.{m}", f'return VH_O.{m}((H) null, null, "x");'))
for m in CAE:
    lines.append(case(f"ref-instance.{m}", f'return VH_O.{m}((H) null, null, "x");'))
for m in GAS:
    lines.append(case(f"ref-instance.{m}", f'return VH_O.{m}((H) null, "x");'))
# getAndAdd* / getAndBitwise* on a REFERENCE variable are deliberately absent.
# They are a SECOND rule, not this one: HotSpot answers
# UnsupportedOperationException for them, because the access-mode support check
# runs before the null check, and CratonVM answers NullPointerException once
# this vector's own rule is in. That is a wrong exception CLASS rather than a
# wrong outcome -- and it was `returned:0` / `returned:null` before, so the
# direction is right -- but making it exact needs its own oracle sweep across
# every variable type (getAndBitwise IS supported for boolean; getAndAdd is
# not), which is a different measurement. Recorded on the page instead of
# smuggled in here.

# ---- array element over `int[]`, null array -------------------------------
for m in READ:
    lines.append(case(f"int-array.{m}", f'return (int) VH_A.{m}((int[]) null, 0);'))
for m in WRITE:
    lines.append(case(f"int-array.{m}", f'VH_A.{m}((int[]) null, 0, 7); return VOID;'))
for m in CAS:
    lines.append(case(f"int-array.{m}", f'return VH_A.{m}((int[]) null, 0, 0, 7);'))
for m in CAE:
    lines.append(case(f"int-array.{m}", f'return (int) VH_A.{m}((int[]) null, 0, 0, 7);'))
for m in GAS + GAA:
    lines.append(case(f"int-array.{m}", f'return (int) VH_A.{m}((int[]) null, 0, 7);'))

# ---- static field, which HAS no coordinate -------------------------------
lines.append(case("int-static.get", 'return (int) VH_S.get();'))
lines.append(case("int-static.set", 'VH_S.set(7); return VOID;'))

body = "".join(lines)

java = '''import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * Every {@code VarHandle} access mode, reached with a NULL coordinate.
 *
 * <p>HotSpot raises {@code NullPointerException} for all of them. CratonVM used
 * to answer instead — {@code null} for a reference read, {@code 0} for a
 * primitive read, and a silent no-op for a write, which is a lost store with no
 * signal anywhere. See
 * varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md (internal).
 *
 * <p>The matrix is generated rather than hand-typed: each row is a
 * signature-polymorphic call site whose exact static types decide which access
 * mode is invoked, so a stray cast silently tests a different mode. The last
 * two rows are the control — a STATIC-field handle takes no coordinate, so
 * there is nothing to be null and both VMs must answer normally.
 */
public class RJdkVarHandleNullCoord {

    static final Object VOID = "void";

    static class H {
        int i = 11;
        Object o = "seed";
    }

    static int stat = 3;

    static final VarHandle VH_I;   // H.i, an int instance field
    static final VarHandle VH_O;   // H.o, an Object instance field
    static final VarHandle VH_A;   // int[] element
    static final VarHandle VH_S;   // static int

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_I = l.findVarHandle(H.class, "i", int.class);
            VH_O = l.findVarHandle(H.class, "o", Object.class);
            VH_A = MethodHandles.arrayElementVarHandle(int[].class);
            VH_S = l.findStaticVarHandle(RJdkVarHandleNullCoord.class, "stat", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Body { Object run() throws Throwable; }

    static int checks = 0;

    /**
     * One row. Prints the OUTCOME — the throwable's class name, or the value
     * that came back instead — and nothing else, so the cross-VM diff is
     * exactly "did the two VMs do the same thing".
     */
    static void probe(String label, Body b) {
        String outcome;
        try {
            Object v = b.run();
            outcome = (v == VOID) ? "returned-normally" : "returned:" + v;
        } catch (Throwable t) {
            outcome = "threw:" + t.getClass().getName();
        }
        checks++;
        System.out.println("CK RJdkVarHandleNullCoord " + label + "=" + outcome);
    }

    public static void main(String[] args) {
%s
        System.out.println("CK RJdkVarHandleNullCoord checks=" + checks);
        System.out.println("PASS RJdkVarHandleNullCoord");
    }
}
''' % body

open("RJdkVarHandleNullCoord.java", "w", encoding="utf-8", newline="\n").write(java)
print("wrote RJdkVarHandleNullCoord.java with", len(lines), "rows")
