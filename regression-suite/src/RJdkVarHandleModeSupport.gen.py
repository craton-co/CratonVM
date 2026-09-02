#!/usr/bin/env python3
"""Generate RJdkVarHandleModeSupport.java.

The rule under test is "which access modes does a VarHandle support, given the
type of the variable" — and it is NOT uniform, which is why this is a generated
sweep and not a handful of hand-picked rows. Every arithmetic and bitwise mode
against every variable type, plus the read-only (final-field) rule and the
ORDER between all three. HotSpot's precedence, measured: read-only beats an
unsupported mode beats a null coordinate -- though the first two both raise
UnsupportedOperationException, so only the second boundary is observable.
"""

# (tag, java type, field name, literal, cast-for-return)
TYPES = [
    ("boolean", "boolean", "z", "true",       "(boolean) "),
    ("byte",    "byte",    "b", "(byte) 1",   "(byte) "),
    ("char",    "char",    "c", "(char) 1",   "(char) "),
    ("short",   "short",   "s", "(short) 1",  "(short) "),
    ("int",     "int",     "i", "1",          "(int) "),
    ("long",    "long",    "j", "1L",         "(long) "),
    ("float",   "float",   "f", "1f",         "(float) "),
    ("double",  "double",  "d", "1d",         "(double) "),
    ("Object",  "Object",  "o", '"x"',        ""),
    ("String",  "String",  "t", '"x"',        "(String) "),
]

ARITH = ["getAndAdd", "getAndAddAcquire", "getAndAddRelease"]
BITS = [f"getAndBitwise{op}{suf}"
        for op in ("Or", "And", "Xor") for suf in ("", "Acquire", "Release")]
MODES = ARITH + BITS

# The remaining families, needed by the read-only section: a read-only handle
# refuses EVERY write mode, not just the arithmetic ones.
READ = ["get", "getVolatile", "getOpaque", "getAcquire"]
WRITE = ["set", "setVolatile", "setOpaque", "setRelease"]
CAS = ["compareAndSet", "weakCompareAndSet", "weakCompareAndSetPlain",
       "weakCompareAndSetAcquire", "weakCompareAndSetRelease"]
CAE = ["compareAndExchange", "compareAndExchangeAcquire",
       "compareAndExchangeRelease"]
GAS = ["getAndSet", "getAndSetAcquire", "getAndSetRelease"]

def case(label, expr):
    return f'        probe("{label}", () -> {{ {expr} }});\n'

lines = []

# ---- the rule itself: every mode x every variable type, LIVE receiver ------
for tag, jt, fld, lit, cast in TYPES:
    for m in MODES:
        lines.append(case(f"{tag}.{m}",
                          f'return {cast}VH_{fld.upper()}.{m}(h, {lit});'))

# ---- order question 1: unsupported mode vs NULL coordinate ----------------
# Measured on 2026-09-02 as UOE-beats-NPE for a reference variable; swept here
# across the types so the ordering is a rule and not one observation.
for tag, jt, fld, lit, cast in TYPES:
    for m in ("getAndAdd", "getAndBitwiseOr"):
        lines.append(case(f"null-recv.{tag}.{m}",
                          f'return {cast}VH_{fld.upper()}.{m}((H) null, {lit});'))

# ---- READ-ONLY handles: a final field ------------------------------------
# `findVarHandle` on a final field yields a handle whose WRITE modes are
# unsupported. Swept over every write family, plus the read modes as the
# control (those stay legal), plus the STATIC final field, whose handle is
# read-only for the same reason by a different lookup.
RO_WRITE = (
    [("set", "VH_FIN.{m}(h, 5); return VOID;")] +
    [(m, "VH_FIN.{m}(h, 5); return VOID;") for m in WRITE[1:]] +
    [(m, "return VH_FIN.{m}(h, 9, 5);") for m in CAS] +
    [(m, "return (int) VH_FIN.{m}(h, 9, 5);") for m in CAE] +
    [(m, "return (int) VH_FIN.{m}(h, 5);") for m in GAS + ARITH] +
    [(m, "return (int) VH_FIN.{m}(h, 5);") for m in BITS]
)
for m, tpl in RO_WRITE:
    lines.append(case(f"final-int.{m}", tpl.format(m=m)))
# the reads must still work -- a read-only handle is read-only, not dead
for m in READ:
    lines.append(case(f"final-int.{m}", f'return (int) VH_FIN.{m}(h);'))

# a final REFERENCE field, to show read-only-ness is about the HANDLE and not
# about the variable's type
for m in ("set", "getAndSet", "compareAndSet"):
    if m == "compareAndSet":
        lines.append(case(f"final-ref.{m}", f'return VH_FINREF.{m}(h, null, "y");'))
    elif m == "set":
        lines.append(case(f"final-ref.{m}", f'VH_FINREF.{m}(h, "y"); return VOID;'))
    else:
        lines.append(case(f"final-ref.{m}", f'return VH_FINREF.{m}(h, "y");'))
lines.append(case("final-ref.get", 'return VH_FINREF.get(h);'))

# a STATIC final field
lines.append(case("static-final.set", 'VH_SFIN.set(5); return VOID;'))
lines.append(case("static-final.getAndSet", 'return (int) VH_SFIN.getAndSet(5);'))
lines.append(case("static-final.get", 'return (int) VH_SFIN.get();'))

# ---- ORDER: read-only vs a NULL coordinate -------------------------------
# The one interaction that is OBSERVABLE. Read-only and unsupported-mode both
# raise UnsupportedOperationException, so their relative order cannot be seen
# from the exception class; read-only against a null coordinate is UOE against
# NullPointerException, and something has to win.
for m in ("set", "getAndSet", "compareAndSet", "getAndAdd"):
    if m == "set":
        lines.append(case(f"null-final.{m}", 'VH_FIN.set((H) null, 5); return VOID;'))
    elif m == "compareAndSet":
        lines.append(case(f"null-final.{m}", 'return VH_FIN.compareAndSet((H) null, 9, 5);'))
    else:
        lines.append(case(f"null-final.{m}", f'return (int) VH_FIN.{m}((H) null, 5);'))

# ---- control: modes every type supports ----------------------------------
for tag, jt, fld, lit, cast in TYPES:
    lines.append(case(f"control.{tag}.getAndSet",
                      f'return {cast}VH_{fld.upper()}.getAndSet(h, {lit});'))
    lines.append(case(f"control.{tag}.compareAndSet",
                      f'return VH_{fld.upper()}.compareAndSet(h, '
                      f'{cast if cast else ""}h2.{fld}, {lit});'))

fields = "\n".join(f"        {jt} {fld};" for _, jt, fld, _, _ in TYPES)
handles = "\n".join(f"    static final VarHandle VH_{fld.upper()};"
                    for _, _, fld, _, _ in TYPES)
inits = "\n".join(
    f'            VH_{fld.upper()} = l.findVarHandle(H.class, "{fld}", {jt}.class);'
    for _, jt, fld, _, _ in TYPES)

body = "".join(lines)

java = '''import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * Which access modes a {@code VarHandle} supports, by VARIABLE TYPE.
 *
 * <p>The rule is not uniform, which is the whole reason this is a generated
 * sweep: {@code getAndBitwise*} is supported for {@code boolean} while
 * {@code getAndAdd*} is not, and neither is supported for a reference. A
 * hand-picked handful of rows cannot tell a rule from a coincidence.
 *
 * <p>The ORDER question is swept alongside it, because an implementation has to
 * pick one and HotSpot's choice is the oracle: an unsupported mode beats a null
 * coordinate, uniformly, for all ten variable types.
 *
 * <p>Each row prints only the outcome — the throwable's class, or the value
 * that came back — so the cross-VM comparison is a plain diff.
 */
public class RJdkVarHandleModeSupport {

    static final Object VOID = "void";

    static class H {
%s
        final int fin = 9;
        final Object finRef = "seed";
    }

    static final int SFIN = 9;

    static final H h = new H();
    static final H h2 = new H();

%s
    static final VarHandle VH_FIN;
    static final VarHandle VH_FINREF;
    static final VarHandle VH_SFIN;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
%s
            VH_FIN = l.findVarHandle(H.class, "fin", int.class);
            VH_FINREF = l.findVarHandle(H.class, "finRef", Object.class);
            VH_SFIN = l.findStaticVarHandle(
                RJdkVarHandleModeSupport.class, "SFIN", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Body { Object run() throws Throwable; }

    static int checks = 0;

    /**
     * Render a value as PURE ASCII.
     *
     * <p>Not cosmetic. A {@code char} variable's modes return values like
     * {@code }, and a raw control byte in the output makes {@code diff}
     * treat the file as BINARY and refuse to compare it line by line — which
     * reports "no differences" for two files that differ on 57 rows. The
     * cross-VM check is a text diff, so the vector owes it text.
     */
    static String show(Object v) {
        String s = String.valueOf(v);
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                out.append(String.format("<U+%%04x>", (int) c));
            } else {
                out.append(c);
            }
        }
        return out.toString();
    }

    static void probe(String label, Body b) {
        String outcome;
        try {
            Object v = b.run();
            outcome = (v == VOID) ? "returned-normally" : "returned:" + show(v);
        } catch (Throwable t) {
            outcome = "threw:" + t.getClass().getName();
        }
        checks++;
        System.out.println("CK RJdkVarHandleModeSupport " + label + "=" + outcome);
    }

    public static void main(String[] args) {
%s
        System.out.println("CK RJdkVarHandleModeSupport checks=" + checks);
        System.out.println("PASS RJdkVarHandleModeSupport");
    }
}
''' % (fields, handles, inits, body)

open("RJdkVarHandleModeSupport.java", "w", encoding="utf-8", newline="\n").write(java)
print("wrote RJdkVarHandleModeSupport.java with", len(lines), "rows")
