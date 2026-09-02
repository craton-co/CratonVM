#!/usr/bin/env python3
"""Generate RJdkVarHandleModeSupport.java.

The rule under test is "which access modes does a VarHandle support, given the
type of the variable" — and it is NOT uniform, which is why this is a generated
sweep and not a handful of hand-picked rows. Every arithmetic and bitwise mode
against every variable type, plus the ORDER question that an implementation
has to answer: does an unsupported mode beat a null coordinate? (HotSpot: yes,
uniformly, across all ten types.)

A third rule found by the same sweep -- a read-only, final-field handle
refuses every WRITE mode -- is recorded in the comment below and deliberately
not asserted here; it is not about the variable's type.
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

# ---- READ-ONLY (final-field) handles are deliberately ABSENT ------------
# A third rule, reached by a different route: `findVarHandle` on a final field
# yields a handle whose WRITE modes are unsupported, and HotSpot answers
# UnsupportedOperationException for set / setVolatile / getAndSet /
# compareAndSet / getAndAdd on one. CratonVM performs the write -- measured,
# `final-int.set` stores 5 into a final field and `getAndSet` returns 5.
#
# Not fixed with this rule and not asserted here, because it is not about the
# variable's TYPE: it needs the handle to record that its field was final,
# which means field-level access flags reaching a native, which is plumbing
# across crates rather than a check. Filed as known-issues/jdk-only/
# varhandle-final-field-handle-performs-the-write-20260902.md, which also
# records the one thing still unmeasured: where a read-only refusal sits
# against the OTHER two rules when a final-field handle is also given a null
# coordinate. Re-add this section when that lands.

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
    }

    static final H h = new H();
    static final H h2 = new H();

%s
    static final VarHandle VH_FIN;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
%s
            VH_FIN = l.findVarHandle(H.class, "fin", int.class);
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
