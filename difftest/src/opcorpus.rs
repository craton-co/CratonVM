// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The **generated opcode corpus** — one Java program per JVMS opcode.
//!
//! [`crate::matrix`] derives coverage from the corpus's own class files, which
//! makes it honest but does not make it *useful*: a matrix over a three-program
//! corpus reports a number that means almost nothing. The committed `seeds/`
//! corpus exercises roughly four of the 202 named opcodes, so ~198 of them are
//! reported as gaps and nobody can tell which gaps matter.
//!
//! This module closes that. It emits, programmatically, one self-contained,
//! deterministic Java program per opcode, built so that the opcode is *forced*
//! into the compiled bytes rather than hoped for.
//!
//! ## Three properties every generated program has
//!
//! 1. **A back-edge and a handler around the focus code.** [`crate::matrix`]'s
//!    code-side gate is per *method*: the `osr` axis needs the opcode to sit in
//!    a method with a backward branch, and the `exception` axis needs a
//!    non-empty exception table. Every program therefore runs its focus code
//!    inside a loop inside a `try`, and every generated *helper* method carries
//!    its own loop and handler, because the facts do not cross a method
//!    boundary. Without this the whole `osr` and `exception` columns stay empty
//!    however many opcodes the corpus gains.
//! 2. **A declared checksum.** Every program prints
//!    `##DIFFTEST-CHECKSUM## <name> <value>` (see [`crate::checksum`]), so the
//!    fifth comparison dimension is live for the whole corpus instead of being
//!    a feature no seed uses. The value is an FNV-1a 64 accumulator folded over
//!    every intermediate, so a wrong value anywhere in the loop changes it.
//! 3. **Determinism.** No wall-clock, no `Object.hashCode`, no map iteration
//!    order, no `String.format` locale surface. Loop bounds are derived from
//!    `args.length` (0 at run time, opaque to `javac`) so the trip count cannot
//!    be constant-folded away, and every printed quantity is specified by the
//!    JLS.
//!
//! ## What cannot be generated, and why that is stated rather than hidden
//!
//! Five opcodes have **no Java source that makes `javac` emit them**
//! ([`OpcodeSupport::UnreachableFromSource`]). They are reported as
//! `unreachable-by-construction` with the reason attached, never as a silent
//! omission and never as an ordinary gap — an ordinary gap says "write a seed",
//! and for these that advice is wrong.
//!
//! ## The witness, and what it is *not*
//!
//! Each entry carries a [`witness`](OpcodeProgram::witness): the raw encoding of
//! the focus instruction(s) the program is built around. It exists so the
//! walker in [`crate::matrix`] can be tested **hermetically** — no JDK, no
//! `javac`, no subprocess — for all 202 opcodes including the three
//! variable-length ones.
//!
//! The witness is a *declaration*, not proof that `javac` emitted the opcode.
//! The proof is the CI reconciliation step: `matrix` compiles the generated
//! corpus with the same `javac` the differential run uses and reports every
//! opcode a generator claims but the compiled bytes do not contain
//! (`CoverageMatrix::reconciliation`). A generator whose recipe stops working —
//! because a `javac` release changed a lowering — shows up there rather than
//! quietly inflating the coverage number.

use crate::matrix::{self, GeneratorIndex, GeneratorStatus};

/// How the corpus is generated. Every field is a pure input: the same config
/// produces byte-identical sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcodeCorpusConfig {
    /// Trip count of the loop wrapped around every program's focus code.
    ///
    /// Under `osr-eager` (`CRATONVM_TIER_OSR_BACKEDGE=1`) one back-edge is
    /// enough; this is sized for the *default* thresholds, so the same corpus
    /// is meaningful without the eager knob.
    pub loop_iterations: u32,
    /// Locals declared by the `wide` program. Must exceed 255 for `javac` to
    /// need the `wide` prefix at all.
    pub wide_locals: usize,
    /// Statements in the `goto_w` program's loop body. Each is 4 bytes of
    /// bytecode, so the default puts the back-edge ~40 KiB away — past the
    /// signed-16-bit branch offset `goto` can encode, and inside `javac`'s
    /// 64 KiB per-method limit.
    pub goto_w_statements: usize,
    /// Distinct string constants the `ldc_w` program interns. Must exceed 255
    /// for any constant-pool index to need the wide `ldc_w` form.
    pub constant_pool_entries: usize,
    /// Emit the two programs whose sources are large by construction (`wide`,
    /// `goto_w`). Off makes generation fast for a smoke run, at the cost of two
    /// opcodes reported as [`OpcodeSupport::Skipped`].
    pub include_large: bool,
}

impl Default for OpcodeCorpusConfig {
    fn default() -> Self {
        Self {
            loop_iterations: 20_000,
            wide_locals: 300,
            goto_w_statements: 10_000,
            constant_pool_entries: 400,
            include_large: true,
        }
    }
}

/// Whether this build can produce a program for an opcode, and why not when it
/// cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpcodeSupport {
    /// A program was generated with this opcode as its focus.
    Generated,
    /// No Java source makes `javac` emit this opcode. Carries the reason, which
    /// is the actionable part: "write a seed" is the wrong advice here.
    UnreachableFromSource(&'static str),
    /// Generatable, but omitted by this [`OpcodeCorpusConfig`].
    Skipped(&'static str),
}

impl OpcodeSupport {
    /// The reason text, for the two non-generated cases.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            OpcodeSupport::Generated => None,
            OpcodeSupport::UnreachableFromSource(r) | OpcodeSupport::Skipped(r) => Some(*r),
        }
    }
}

/// One opcode's entry in the generated corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcodeProgram {
    pub opcode: u8,
    /// The JVMS mnemonic ([`matrix::opcode_name`]).
    pub mnemonic: &'static str,
    /// The `public class` name and the `.java` file stem, e.g. `Op096_iadd`.
    /// Mnemonics that are Java keywords (`goto`, `new`, `return`, `instanceof`)
    /// are safe here because the numeric prefix makes the whole thing an
    /// ordinary identifier.
    pub class_name: String,
    pub support: OpcodeSupport,
    /// The Java source, when [`OpcodeSupport::Generated`].
    pub source: Option<String>,
    /// The `<name>` this program declares its checksum under.
    pub checksum_name: String,
    /// Raw encoding of the focus instruction(s). See the module docs: a
    /// declaration for hermetic tests, not proof of what `javac` emitted.
    pub witness: Vec<u8>,
    /// The operand forms `witness` decodes to, via [`matrix::operand_form`] —
    /// derived, never hand-written, so the two cannot drift.
    pub forms: Vec<String>,
}

impl OpcodeProgram {
    /// Whether a `.java` file should be written for this entry.
    pub fn is_generated(&self) -> bool {
        self.support == OpcodeSupport::Generated
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Every opcode's entry, in opcode order. Reserved opcodes (`0xca`..) are not
/// included: they have no JVMS mnemonic and no meaning to cover.
///
/// Pure: no PRNG, no clock, no filesystem. `generate_all(c) == generate_all(c)`.
pub fn generate_all(config: &OpcodeCorpusConfig) -> Vec<OpcodeProgram> {
    (0u8..=0xc9)
        .filter_map(|op| matrix::opcode_name(op).map(|m| (op, m)))
        .map(|(opcode, mnemonic)| {
            let class_name = format!("Op{opcode:03}_{mnemonic}");
            let checksum_name = format!("op{opcode:03}-{mnemonic}");
            let witness = witness_bytes(opcode);
            let forms = witness_forms(&witness);
            let (support, source) = match recipe(opcode, config) {
                Recipe::Emit { members, body } => (
                    OpcodeSupport::Generated,
                    Some(assemble(
                        &class_name,
                        mnemonic,
                        opcode,
                        &checksum_name,
                        config,
                        &members,
                        &body,
                    )),
                ),
                Recipe::Unreachable(reason) => (OpcodeSupport::UnreachableFromSource(reason), None),
                Recipe::Skipped(reason) => (OpcodeSupport::Skipped(reason), None),
            };
            OpcodeProgram {
                opcode,
                mnemonic,
                class_name,
                support,
                source,
                checksum_name,
                witness,
                forms,
            }
        })
        .collect()
}

/// The generator's declarations, in the shape [`crate::matrix`] consumes.
///
/// Deliberately narrow: the matrix learns *whether* an opcode has a generator
/// and *why* it does not, and nothing else. Keeping it to that means `matrix`
/// has no dependency on this module (the arrow points one way, here → matrix),
/// so the coverage report still builds for a corpus this generator never
/// touched.
pub fn generator_index(config: &OpcodeCorpusConfig) -> GeneratorIndex {
    generate_all(config)
        .into_iter()
        .map(|p| {
            let status = match p.support {
                OpcodeSupport::Generated => GeneratorStatus::Generated,
                OpcodeSupport::UnreachableFromSource(reason) | OpcodeSupport::Skipped(reason) => {
                    GeneratorStatus::Unreachable {
                        reason: reason.to_string(),
                    }
                }
            };
            (p.opcode, status)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The program template
// ---------------------------------------------------------------------------

/// The one shape every generated program has. Placeholders are `@…@` rather
/// than `{…}` so the Java braces need no escaping and the template reads as
/// Java.
const TEMPLATE: &str = r###"// difftest: strict (generated: opcode @HEX@ @MNEMONIC@)
//
// Emitted by `cratonvm-difftest gen-opcodes`; do not edit by hand.
//
// Focus opcode: `@MNEMONIC@` (@HEX@). The focus code runs inside a loop, so the
// method has a back-edge and the `osr` axis has a code site, and inside a
// handler, so the method has a non-empty exception table and the `exception`
// axis has one. Every intermediate is folded into a declared checksum, which is
// read from un-normalized stdout and so cannot be laundered by a normalization
// rule. Deterministic by construction: no wall-clock, no identity hash, no map
// iteration order.
public class @CLASS@ {
    // FNV-1a 64: the offset basis, then the per-value step in `mix`. This
    // spelling is what `checksum::fnv1a64_hex` computes, reproduced in five
    // lines of Java without touching java.security, whose provider stack is
    // itself one of the things under test.
    static long acc = 0xcbf29ce484222325L;

    static void mix(long v) {
        acc ^= v;
        acc *= 0x100000001b3L;
    }
@MEMBERS@
    public static void main(String[] args) {
        // `args.length` is 0 at run time and opaque to javac, so the trip count
        // survives constant folding and the loop is really executed.
        int trips = @ITERS@ + args.length;
        try {
            for (int i = 0; i < trips; i++) {
@BODY@
            }
        } catch (Throwable caught) {
            mix(caught.getClass().getName().length());
        }
        System.out.println("@CLASS@ acc=" + Long.toHexString(acc));
        System.out.println("##DIFFTEST-CHECKSUM## @CHECKSUM@ " + Long.toHexString(acc));
    }
}
"###;

/// Indent every non-blank line of `text` by `spaces`.
fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn assemble(
    class_name: &str,
    mnemonic: &str,
    opcode: u8,
    checksum_name: &str,
    config: &OpcodeCorpusConfig,
    members: &str,
    body: &str,
) -> String {
    let members_block = if members.trim().is_empty() {
        String::new()
    } else {
        format!("\n{}\n", indent(members, 4))
    };
    TEMPLATE
        .replace("@MEMBERS@", &members_block)
        .replace("@BODY@", &indent(body, 16))
        .replace("@CLASS@", class_name)
        .replace("@MNEMONIC@", mnemonic)
        .replace("@HEX@", &format!("{opcode:#04x}"))
        .replace("@CHECKSUM@", checksum_name)
        .replace("@ITERS@", &config.loop_iterations.to_string())
}

/// A helper method whose body carries the focus opcode.
///
/// It gets its own loop and its own handler because [`matrix::SiteFacts`] are
/// per method: an opcode pushed into a helper to get the operand shape right
/// (a parameter at slot 0, a specific return type) would otherwise lose both
/// the `osr` and the `exception` code site that `main`'s wrapper provides.
fn helper(name: &str, sig: &str, decls: &str, body: &str) -> String {
    let decl_block = if decls.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", indent(decls, 4))
    };
    format!(
        "static long {name}({sig}) {{\n\
         {decl_block}    long s = 0;\n\
         \x20   try {{\n\
         \x20       for (int k = 0; k < 3; k++) {{\n\
         {body}\n\
         \x20       }}\n\
         \x20   }} catch (RuntimeException ex) {{\n\
         \x20       s = -1;\n\
         \x20   }}\n\
         \x20   return s;\n\
         }}",
        body = indent(body, 12)
    )
}

// ---------------------------------------------------------------------------
// Recipes
// ---------------------------------------------------------------------------

enum Recipe {
    Emit { members: String, body: String },
    Unreachable(&'static str),
    Skipped(&'static str),
}

fn emit(members: &str, body: &str) -> Recipe {
    Recipe::Emit {
        members: members.to_string(),
        body: body.to_string(),
    }
}

const REASON_NOP: &str = "javac emits no `nop`: no Java construct produces one, and it carries no \
                          semantics to express. It reaches a class file only through the `mutate` \
                          tier or a hand-assembled class — cover it there, not with a seed.";
const REASON_SWAP: &str = "javac's code generator never emits `swap`; it reorders the operand \
                           stack with `dup_x1`/`dup_x2`/`pop` instead. Only a hand-assembled or \
                           mutated class file contains one.";
const REASON_JSR: &str = "`jsr`/`ret` implement the pre-Java-6 `finally` subroutine. javac has \
                          not emitted them since 1.5, and JVMS §4.9.1 forbids them outright in \
                          class files of version >= 50.0, so no compilable source can produce \
                          one. Only a hand-assembled version <= 49 class file can.";
const REASON_LARGE: &str = "generatable, but omitted by this configuration (--no-large): the \
                            recipe needs a method body large enough to force the wide encoding.";

fn recipe(opcode: u8, config: &OpcodeCorpusConfig) -> Recipe {
    match opcode {
        // -- constants ------------------------------------------------------
        0x00 => Recipe::Unreachable(REASON_NOP),
        0x01 => emit("", "Object o = null;\nmix(o == null ? 1 : 0);"),
        0x02 => emit("", "int v = -1;\nmix(v ^ i);"),
        // iconst_0 .. iconst_5. The literal is bound to an `int` local first:
        // `mix(3)` would be folded to a *long* constant by javac (the parameter
        // is `long`), and emit `ldc2_w`, not `iconst_3`.
        0x03..=0x08 => {
            let k = opcode - 0x03;
            emit("", &format!("int v = {k};\nmix(v ^ i);"))
        }
        0x09 | 0x0a => {
            let k = opcode - 0x09;
            emit("", &format!("long v = {k}L;\nmix(v ^ i);"))
        }
        0x0b..=0x0d => {
            let k = opcode - 0x0b;
            emit("", &format!("float v = {k}f;\nmix((long) (v + i));"))
        }
        0x0e | 0x0f => {
            let k = opcode - 0x0e;
            emit("", &format!("double v = {k}d;\nmix((long) (v + i));"))
        }
        0x10 => emit("", "int v = 100;\nmix(v ^ i);"),
        0x11 => emit("", "int v = 30000;\nmix(v ^ i);"),
        0x12 => emit("", "int v = 100000;\nmix(v ^ i);"),
        0x13 => ldc_w_recipe(config),
        0x14 => emit("", "long v = 1234567890123L;\nmix(v ^ i);"),

        // -- indexed loads / stores (slot >= 4, so not the _N short forms) ---
        0x15 | 0x36 => emit(&many_int_locals(), "mix(manyIntLocals(i));"),
        0x16 | 0x37 => emit(&many_long_locals(), "mix(manyLongLocals(i));"),
        0x17 | 0x38 => emit(&many_float_locals(), "mix(manyFloatLocals(i));"),
        0x18 | 0x39 => emit(&many_double_locals(), "mix(manyDoubleLocals(i));"),
        0x19 | 0x3a => emit(&many_ref_locals(), "mix(manyRefLocals(i));"),

        // -- the _N short forms: parameters pinned to slots 0..3 -------------
        0x1a..=0x1d => emit(
            &helper(
                "i4",
                "int a, int b, int c, int d",
                "",
                "s += a + b + c + d;",
            ),
            "mix(i4(i, i + 1, i + 2, i + 3));",
        ),
        0x1e..=0x21 => emit(
            &long_slot_helpers(),
            "mix(l02(i, i + 1) + l13(i, i, i + 1));",
        ),
        0x22..=0x25 => emit(
            &helper(
                "f4",
                "float a, float b, float c, float d",
                "",
                "s += (long) (a + b + c + d);",
            ),
            "mix(f4(i, i + 1, i + 2, i + 3));",
        ),
        0x26..=0x29 => emit(
            &double_slot_helpers(),
            "mix(d02(i, i + 1) + d13(i, i, i + 1));",
        ),
        0x2a..=0x2d => emit(
            &helper(
                "a4",
                "String a, String b, String c, String d",
                "",
                "s += a.length() + b.length() + c.length() + d.length();",
            ),
            "mix(a4(\"a\", \"bb\", \"ccc\", \"dddd\"));",
        ),

        // -- array loads ----------------------------------------------------
        0x2e => emit(
            "static final int[] ARI = { 1, 2, 3, 4 };",
            "mix(ARI[i & 3]);",
        ),
        0x2f => emit(
            "static final long[] ARJ = { 1L, 2L, 3L, 4L };",
            "mix(ARJ[i & 3]);",
        ),
        0x30 => emit(
            "static final float[] ARF = { 1f, 2f, 3f, 4f };",
            "mix((long) ARF[i & 3]);",
        ),
        0x31 => emit(
            "static final double[] ARD = { 1d, 2d, 3d, 4d };",
            "mix((long) ARD[i & 3]);",
        ),
        0x32 => emit(
            "static final String[] ARS = { \"a\", \"bb\", \"ccc\", \"dddd\" };",
            "mix(ARS[i & 3].length());",
        ),
        0x33 => emit(
            "static final byte[] ARB = { 1, 2, 3, 4 };",
            "mix(ARB[i & 3]);",
        ),
        0x34 => emit(
            "static final char[] ARC = { 'a', 'b', 'c', 'd' };",
            "mix(ARC[i & 3]);",
        ),
        0x35 => emit(
            "static final short[] ARH = { 1, 2, 3, 4 };",
            "mix(ARH[i & 3]);",
        ),

        // -- the _N store short forms: assign back to a slot-0..3 parameter --
        0x3b..=0x3e => emit(
            &helper(
                "is4",
                "int a, int b, int c, int d",
                "",
                "a = k;\nb = k + 1;\nc = k + 2;\nd = k + 3;\ns += a + b + c + d;",
            ),
            "mix(is4(i, i + 1, i + 2, i + 3));",
        ),
        0x3f..=0x42 => emit(
            &long_store_helpers(),
            "mix(ls02(i, i + 1) + ls13(i, i, i + 1));",
        ),
        0x43..=0x46 => emit(
            &helper(
                "fs4",
                "float a, float b, float c, float d",
                "",
                "a = k;\nb = k + 1;\nc = k + 2;\nd = k + 3;\ns += (long) (a + b + c + d);",
            ),
            "mix(fs4(i, i + 1, i + 2, i + 3));",
        ),
        0x47..=0x4a => emit(
            &double_store_helpers(),
            "mix(ds02(i, i + 1) + ds13(i, i, i + 1));",
        ),
        0x4b..=0x4e => emit(
            &helper(
                "as4",
                "String a, String b, String c, String d",
                "",
                "a = \"w\";\nb = \"xx\";\nc = \"yyy\";\nd = \"zzzz\";\n\
                 s += a.length() + b.length() + c.length() + d.length();",
            ),
            "mix(as4(\"a\", \"bb\", \"ccc\", \"dddd\"));",
        ),

        // -- array stores ---------------------------------------------------
        0x4f => emit(
            "static final int[] WI = new int[4];",
            "WI[i & 3] = i;\nmix(WI[i & 3]);",
        ),
        0x50 => emit(
            "static final long[] WJ = new long[4];",
            "WJ[i & 3] = i;\nmix(WJ[i & 3]);",
        ),
        0x51 => emit(
            "static final float[] WF = new float[4];",
            "WF[i & 3] = i;\nmix((long) WF[i & 3]);",
        ),
        0x52 => emit(
            "static final double[] WD = new double[4];",
            "WD[i & 3] = i;\nmix((long) WD[i & 3]);",
        ),
        0x53 => emit(
            "static final String[] WA = new String[4];",
            "WA[i & 3] = \"v\";\nmix(WA[i & 3].length());",
        ),
        0x54 => emit(
            "static final byte[] WB = new byte[4];",
            "WB[i & 3] = (byte) i;\nmix(WB[i & 3]);",
        ),
        0x55 => emit(
            "static final char[] WC = new char[4];",
            "WC[i & 3] = (char) i;\nmix(WC[i & 3]);",
        ),
        0x56 => emit(
            "static final short[] WH = new short[4];",
            "WH[i & 3] = (short) i;\nmix(WH[i & 3]);",
        ),

        // -- operand-stack shuffles -----------------------------------------
        // A call whose result is discarded: one slot -> `pop`, two -> `pop2`.
        0x57 => emit("static int retInt(int p) { return p + 1; }", "retInt(i);"),
        0x58 => emit(
            "static long retLong(int p) { return p + 1L; }",
            "retLong(i);",
        ),
        0x59 => emit("", "Object o = new Object();\nmix(o == null ? 0 : 1);"),
        // An assignment *used as a value*: javac duplicates the value under the
        // reference/index it is about to consume. The receiver shape picks the
        // dup variant, which is why each of these is a different member set.
        0x5a => emit(INSTANCE_MEMBERS, "int v = (INST.fi = i);\nmix(v);"),
        0x5b => emit(
            "static final int[] DXI = new int[4];",
            "int v = (DXI[i & 3] = i);\nmix(v);",
        ),
        0x5c => emit("static long SJ;", "long v = (SJ = i);\nmix(v);"),
        0x5d => emit(INSTANCE_MEMBERS, "long v = (INST.fj = i);\nmix(v);"),
        0x5e => emit(
            "static final long[] DXJ = new long[4];",
            "long v = (DXJ[i & 3] = i);\nmix(v);",
        ),
        0x5f => Recipe::Unreachable(REASON_SWAP),

        // -- arithmetic: {add,sub,mul,div,rem} x {int,long,float,double} -----
        0x60..=0x73 => {
            const OPS: [&str; 5] = ["+", "-", "*", "/", "%"];
            let sym = OPS[((opcode - 0x60) / 4) as usize];
            match (opcode - 0x60) % 4 {
                // `b` is a local, never a literal, so `javac` cannot fold the
                // division away and `idiv`/`irem` really appear.
                0 => emit("", &format!("int a = i, b = 3;\nmix(a {sym} b);")),
                1 => emit("", &format!("long a = i, b = 3L;\nmix(a {sym} b);")),
                2 => emit(
                    "",
                    &format!("float a = i, b = 3f;\nmix((long) (a {sym} b));"),
                ),
                _ => emit(
                    "",
                    &format!("double a = i, b = 3d;\nmix((long) (a {sym} b));"),
                ),
            }
        }
        0x74 => emit("", "int a = i;\nmix(-a);"),
        0x75 => emit("", "long a = i;\nmix(-a);"),
        0x76 => emit("", "float a = i;\nmix((long) (-a));"),
        0x77 => emit("", "double a = i;\nmix((long) (-a));"),

        // -- shifts and bitwise ---------------------------------------------
        0x78 => emit("", "int a = i, b = 3;\nmix(a << b);"),
        0x79 => emit("", "long a = i;\nint b = 3;\nmix(a << b);"),
        0x7a => emit("", "int a = i, b = 3;\nmix(a >> b);"),
        0x7b => emit("", "long a = i;\nint b = 3;\nmix(a >> b);"),
        0x7c => emit("", "int a = i, b = 3;\nmix(a >>> b);"),
        0x7d => emit("", "long a = i;\nint b = 3;\nmix(a >>> b);"),
        0x7e => emit("", "int a = i, b = 3;\nmix(a & b);"),
        0x7f => emit("", "long a = i, b = 3L;\nmix(a & b);"),
        0x80 => emit("", "int a = i, b = 3;\nmix(a | b);"),
        0x81 => emit("", "long a = i, b = 3L;\nmix(a | b);"),
        0x82 => emit("", "int a = i, b = 3;\nmix(a ^ b);"),
        0x83 => emit("", "long a = i, b = 3L;\nmix(a ^ b);"),
        0x84 => emit("", "int c = i;\nc += 3;\nmix(c);"),

        // -- conversions ----------------------------------------------------
        0x85 => emit("", "int a = i;\nlong b = a;\nmix(b);"),
        0x86 => emit("", "int a = i;\nfloat b = a;\nmix((long) b);"),
        0x87 => emit("", "int a = i;\ndouble b = a;\nmix((long) b);"),
        0x88 => emit("", "long a = i;\nint b = (int) a;\nmix(b);"),
        0x89 => emit("", "long a = i;\nfloat b = a;\nmix((long) b);"),
        0x8a => emit("", "long a = i;\ndouble b = a;\nmix((long) b);"),
        0x8b => emit("", "float a = i;\nint b = (int) a;\nmix(b);"),
        0x8c => emit("", "float a = i;\nlong b = (long) a;\nmix(b);"),
        0x8d => emit("", "float a = i;\ndouble b = a;\nmix((long) b);"),
        0x8e => emit("", "double a = i;\nint b = (int) a;\nmix(b);"),
        0x8f => emit("", "double a = i;\nlong b = (long) a;\nmix(b);"),
        0x90 => emit("", "double a = i;\nfloat b = (float) a;\nmix((long) b);"),
        0x91 => emit("", "int a = i;\nbyte b = (byte) a;\nmix(b);"),
        0x92 => emit("", "int a = i;\nchar b = (char) a;\nmix(b);"),
        0x93 => emit("", "int a = i;\nshort b = (short) a;\nmix(b);"),

        // -- comparisons ----------------------------------------------------
        // Both polarities, deliberately: which of the l/g variants javac picks
        // for `<` and for `>` is a compiler detail, and a recipe that assumed
        // one direction would silently stop covering the other.
        0x94 => emit(
            "",
            "long a = i, b = 3L;\nmix(a < b ? 1 : 0);\nmix(a > b ? 1 : 0);",
        ),
        0x95 | 0x96 => emit(
            "",
            "float a = i, b = 3f;\nmix(a < b ? 1 : 0);\nmix(a > b ? 1 : 0);",
        ),
        0x97 | 0x98 => emit(
            "",
            "double a = i, b = 3d;\nmix(a < b ? 1 : 0);\nmix(a > b ? 1 : 0);",
        ),

        // -- branches -------------------------------------------------------
        // javac emits the *negated* test to jump over the then-block, so a
        // recipe that wrote only `a == 0` would cover `ifne` and never `ifeq`.
        // All six relations in both senses cover all six opcodes whichever way
        // the compiler inverts them.
        0x99..=0x9e => emit("", IF_ZERO_BODY),
        0x9f..=0xa4 => emit("", IF_ICMP_BODY),
        0xa5 | 0xa6 => emit(
            "static final String[] ARS = { \"a\", \"bb\", \"ccc\", \"dddd\" };",
            "String p = ARS[i & 3], q = ARS[(i + 1) & 3];\n\
             if (p == q) { mix(1); }\n\
             if (p != q) { mix(2); }",
        ),
        0xa7 => emit("", "if ((i & 1) == 0) { mix(1); } else { mix(2); }"),
        0xa8 | 0xa9 => Recipe::Unreachable(REASON_JSR),

        // -- switches: dense picks tableswitch, sparse picks lookupswitch ----
        0xaa => emit("", TABLESWITCH_BODY),
        0xab => emit("", LOOKUPSWITCH_BODY),

        // -- returns --------------------------------------------------------
        0xac => emit(
            &typed_return("int", "int", "s += p + k;"),
            "mix(retint(i));",
        ),
        0xad => emit(
            &typed_return("long", "long", "s += p + k;"),
            "mix(retlong(i));",
        ),
        0xae => emit(
            &typed_return("float", "float", "s += p + k;"),
            "mix((long) retfloat(i));",
        ),
        0xaf => emit(
            &typed_return("double", "double", "s += p + k;"),
            "mix((long) retdouble(i));",
        ),
        0xb0 => emit(RETURN_REF_HELPER, "mix(retref(i).length());"),
        0xb1 => emit(RETURN_VOID_HELPER, "retvoid(i);"),

        // -- fields and invocations -----------------------------------------
        0xb2 => emit("static int GS = 1;", "mix(GS + i);"),
        0xb3 => emit("static int PS;", "PS = i;\nmix(PS);"),
        0xb4 => emit(INSTANCE_MEMBERS, "mix(INST.fi + i);"),
        0xb5 => emit(INSTANCE_MEMBERS, "INST.fi = i;\nmix(INST.fi);"),
        0xb6 => emit(
            "static final String[] ARS = { \"a\", \"bb\", \"ccc\", \"dddd\" };",
            "mix(ARS[i & 3].length());",
        ),
        0xb7 => emit("", "Object o = new Object();\nmix(o == null ? 0 : 1);"),
        0xb8 => emit("", "mix(Math.abs(-i));"),
        0xb9 => emit(
            "static final java.util.List<String> LIST =\n\
             \x20       java.util.Arrays.asList(\"a\", \"bb\", \"ccc\", \"dddd\");",
            "mix(LIST.get(i & 3).length() + LIST.size());",
        ),
        // String concatenation lowers to an invokedynamic call site against
        // StringConcatFactory on every JDK this harness targets.
        0xba => emit("", "String s = \"v\" + i;\nmix(s.length());"),

        // -- allocation, casts, monitors ------------------------------------
        0xbb => emit("", "Object o = new Object();\nmix(o == null ? 0 : 1);"),
        0xbc => emit("", NEWARRAY_BODY),
        0xbd => emit("", "String[] a = new String[i & 3];\nmix(a.length);"),
        0xbe => emit("", "int[] a = new int[i & 3];\nmix(a.length);"),
        0xbf => emit("", ATHROW_BODY),
        0xc0 => emit(
            "static final String[] ARS = { \"a\", \"bb\", \"ccc\", \"dddd\" };",
            "Object o = ARS[i & 3];\nString s = (String) o;\nmix(s.length());",
        ),
        0xc1 => emit(
            "static final String[] ARS = { \"a\", \"bb\", \"ccc\", \"dddd\" };",
            "Object o = ARS[i & 3];\nmix(o instanceof String ? 1 : 0);",
        ),
        // javac emits monitorenter plus *two* monitorexits (normal and the
        // synthetic any-handler), so one recipe covers both opcodes.
        0xc2 | 0xc3 => emit(
            "static final Object LOCK = new Object();",
            "synchronized (LOCK) {\n    mix(i);\n}",
        ),

        0xc4 => wide_recipe(config),
        0xc5 => emit("", MULTIANEWARRAY_BODY),
        0xc6 | 0xc7 => emit("", IFNULL_BODY),
        0xc8 => goto_w_recipe(config),
        0xc9 => Recipe::Unreachable(REASON_JSR),

        _ => Recipe::Unreachable("reserved opcode: no JVMS mnemonic"),
    }
}

const INSTANCE_MEMBERS: &str = "int fi;\nlong fj;\nstatic @CLASS@ INST = new @CLASS@();";

const IF_ZERO_BODY: &str = "int a = (i & 3) - 1;\n\
                            if (a == 0) { mix(1); }\n\
                            if (a != 0) { mix(2); }\n\
                            if (a < 0) { mix(3); }\n\
                            if (a >= 0) { mix(4); }\n\
                            if (a > 0) { mix(5); }\n\
                            if (a <= 0) { mix(6); }";

const IF_ICMP_BODY: &str = "int a = i & 3, b = (i >> 2) & 3;\n\
                            if (a == b) { mix(1); }\n\
                            if (a != b) { mix(2); }\n\
                            if (a < b) { mix(3); }\n\
                            if (a >= b) { mix(4); }\n\
                            if (a > b) { mix(5); }\n\
                            if (a <= b) { mix(6); }";

const TABLESWITCH_BODY: &str = "switch (i & 7) {\n\
                                case 0: mix(10); break;\n\
                                case 1: mix(11); break;\n\
                                case 2: mix(12); break;\n\
                                case 3: mix(13); break;\n\
                                case 4: mix(14); break;\n\
                                case 5: mix(15); break;\n\
                                case 6: mix(16); break;\n\
                                case 7: mix(17); break;\n\
                                default: mix(18); break;\n\
                                }";

const LOOKUPSWITCH_BODY: &str = "switch (i & 1023) {\n\
                                 case 1: mix(21); break;\n\
                                 case 100: mix(22); break;\n\
                                 case 1000: mix(23); break;\n\
                                 default: mix(24); break;\n\
                                 }";

const RETURN_REF_HELPER: &str = "static String retref(int p) {\n\
                                 \x20   String s = \"r\";\n\
                                 \x20   try {\n\
                                 \x20       for (int k = 0; k < 3; k++) {\n\
                                 \x20           s = (p + k) < 0 ? \"neg\" : \"pos\";\n\
                                 \x20       }\n\
                                 \x20   } catch (RuntimeException ex) {\n\
                                 \x20       s = \"ex\";\n\
                                 \x20   }\n\
                                 \x20   return s;\n\
                                 }";

const RETURN_VOID_HELPER: &str = "static void retvoid(int p) {\n\
                                  \x20   try {\n\
                                  \x20       for (int k = 0; k < 3; k++) {\n\
                                  \x20           mix(p + k);\n\
                                  \x20       }\n\
                                  \x20   } catch (RuntimeException ex) {\n\
                                  \x20       mix(-1);\n\
                                  \x20   }\n\
                                  }";

const NEWARRAY_BODY: &str = "boolean[] z = new boolean[i & 3];\n\
                             char[] c = new char[i & 3];\n\
                             float[] f = new float[i & 3];\n\
                             double[] d = new double[i & 3];\n\
                             byte[] b = new byte[i & 3];\n\
                             short[] h = new short[i & 3];\n\
                             int[] w = new int[i & 3];\n\
                             long[] j = new long[i & 3];\n\
                             mix(z.length + c.length + f.length + d.length\n\
                             \x20       + b.length + h.length + w.length + j.length);";

const ATHROW_BODY: &str = "try {\n\
                           \x20   if ((i & 1023) == 0) {\n\
                           \x20       throw new IllegalStateException(\"boom\");\n\
                           \x20   }\n\
                           } catch (RuntimeException ex) {\n\
                           \x20   mix(ex.getMessage().length());\n\
                           }";

const MULTIANEWARRAY_BODY: &str = "int[][] m2 = new int[2][i & 3];\n\
                                   int[][][] m3 = new int[2][2][i & 3];\n\
                                   mix(m2.length + m3.length + m2[0].length + m3[0][0].length);";

const IFNULL_BODY: &str = "Object o = ((i & 1) == 0) ? null : \"x\";\n\
                           if (o == null) { mix(1); }\n\
                           if (o != null) { mix(2); }";

fn many_int_locals() -> String {
    helper(
        "manyIntLocals",
        "int p",
        "int a = p, b = p + 1, c = p + 2, d = p + 3, e = p + 4, f = p + 5, g = p + 6;",
        "s += a + b + c + d + e + f + g;",
    )
}

fn many_long_locals() -> String {
    helper(
        "manyLongLocals",
        "int p",
        "long a = p, b = p + 1, c = p + 2;",
        "s += a + b + c;",
    )
}

fn many_float_locals() -> String {
    helper(
        "manyFloatLocals",
        "int p",
        "float a = p, b = p + 1, c = p + 2, d = p + 3, e = p + 4, f = p + 5;",
        "s += (long) (a + b + c + d + e + f);",
    )
}

fn many_double_locals() -> String {
    helper(
        "manyDoubleLocals",
        "int p",
        "double a = p, b = p + 1, c = p + 2;",
        "s += (long) (a + b + c);",
    )
}

fn many_ref_locals() -> String {
    helper(
        "manyRefLocals",
        "int p",
        "String a = \"a\", b = \"bb\", c = \"ccc\", d = \"dddd\", e = \"eeeee\", f = \"ffffff\";",
        "s += a.length() + b.length() + c.length() + d.length() + e.length() + f.length() + p;",
    )
}

/// `lload_0`/`lload_2` come from a `(long, long)` signature; `lload_1`/`lload_3`
/// need a one-slot parameter in front to shift the category-2 values off the
/// even slots. Both helpers ship in the same program so all four short forms
/// are present whichever one the walker looks for.
fn long_slot_helpers() -> String {
    format!(
        "{}\n\n{}",
        helper("l02", "long a, long b", "", "s += a + b;"),
        helper("l13", "int p, long a, long b", "", "s += a + b + p;")
    )
}

fn double_slot_helpers() -> String {
    format!(
        "{}\n\n{}",
        helper("d02", "double a, double b", "", "s += (long) (a + b);"),
        helper(
            "d13",
            "int p, double a, double b",
            "",
            "s += (long) (a + b + p);"
        )
    )
}

fn long_store_helpers() -> String {
    format!(
        "{}\n\n{}",
        helper(
            "ls02",
            "long a, long b",
            "",
            "a = k;\nb = k + 1;\ns += a + b;"
        ),
        helper(
            "ls13",
            "int p, long a, long b",
            "",
            "a = k;\nb = k + 1;\ns += a + b + p;"
        )
    )
}

fn double_store_helpers() -> String {
    format!(
        "{}\n\n{}",
        helper(
            "ds02",
            "double a, double b",
            "",
            "a = k;\nb = k + 1;\ns += (long) (a + b);"
        ),
        helper(
            "ds13",
            "int p, double a, double b",
            "",
            "a = k;\nb = k + 1;\ns += (long) (a + b + p);"
        )
    )
}

/// A helper whose *return type* selects the return opcode. `s` is declared with
/// the target type so the returned expression needs no conversion, and the loop
/// plus handler keep the `osr` and `exception` code sites.
fn typed_return(ty: &str, name_suffix: &str, body: &str) -> String {
    format!(
        "static {ty} ret{name_suffix}(int p) {{\n\
         \x20   {ty} s = 0;\n\
         \x20   try {{\n\
         \x20       for (int k = 0; k < 3; k++) {{\n\
         \x20           {body}\n\
         \x20       }}\n\
         \x20   }} catch (RuntimeException ex) {{\n\
         \x20       s = -1;\n\
         \x20   }}\n\
         \x20   return s;\n\
         }}"
    )
}

/// `ldc_w` is `ldc` with a 16-bit constant-pool index, so it appears only once
/// the pool is deeper than 255 entries *and* a deep entry is actually loaded.
/// Generation is what makes this cheap: interning several hundred distinct
/// string constants is a loop here and would be unreviewable by hand.
fn ldc_w_recipe(config: &OpcodeCorpusConfig) -> Recipe {
    let n = config.constant_pool_entries.max(300);
    let mut literals = String::new();
    for k in 0..n {
        if k % 8 == 0 {
            literals.push_str("\n    ");
        }
        literals.push_str(&format!("\"k{k}\", "));
    }
    let body = format!(
        "String[] pool = {{{literals}\n}};\n\
         s += pool[(p + k) & 255].length() + pool[{last}].length();",
        last = n - 1
    );
    let members = helper("manyConstants", "int p", "", &body);
    Recipe::Emit {
        members,
        // Called on a fraction of the trips: the recipe's cost is the array of
        // several hundred constants, and the opcode is covered either way.
        body: "if ((i & 255) == 0) { mix(manyConstants(i)); }".to_string(),
    }
}

/// `wide` prefixes a load/store/`iinc` whose local slot does not fit in a byte,
/// so the recipe is "declare more than 255 locals and then use the last ones".
/// One long, one double and one reference local sit past the boundary too, so
/// the program covers `wide:lload`, `wide:dload` and `wide:aload` as well as
/// the int forms.
fn wide_recipe(config: &OpcodeCorpusConfig) -> Recipe {
    if !config.include_large {
        return Recipe::Skipped(REASON_LARGE);
    }
    let n = config.wide_locals.max(260);
    let mut decls = String::new();
    for k in 0..n {
        decls.push_str(&format!("int v{k} = p + {k};\n"));
    }
    decls.push_str("long q = p;\ndouble r = p;\nString t = \"t\";");
    let body = format!(
        "v{a} += 1000;\n\
         s += v{a} + v{b} + v{c} + q + (long) r + t.length();",
        a = n - 1,
        b = n - 2,
        c = n - 3
    );
    Recipe::Emit {
        members: helper("wideLocals", "int p", &decls, &body),
        body: "mix(wideLocals(i));".to_string(),
    }
}

/// `goto_w` is emitted only when a branch offset does not fit in a signed 16-bit
/// field, which needs a method body larger than 32 KiB. `javac` regenerates such
/// a method in "fat code" mode and widens its branches, so a loop whose body is
/// ~40 KiB of bytecode produces a `goto_w` back-edge. Each `s += 1;` is four
/// bytes (`lload_1; lconst_1; ladd; lstore_1`), so the statement count is the
/// knob, and the default stays well inside `javac`'s 64 KiB per-method limit.
fn goto_w_recipe(config: &OpcodeCorpusConfig) -> Recipe {
    if !config.include_large {
        return Recipe::Skipped(REASON_LARGE);
    }
    let n = config.goto_w_statements.max(9_000);
    let mut body = String::from("s += p;\n");
    for _ in 0..n {
        body.push_str("s += 1;\n");
    }
    body.push_str("s += k;");
    Recipe::Emit {
        members: helper("farBranch", "int p", "", &body),
        // Called rarely: the method is huge, and one call per 4096 trips still
        // executes it thousands of times over the corpus's loop.
        body: "if ((i & 4095) == 0) { mix(farBranch(i)); }".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Witnesses: hermetic fixtures for the matrix walker
// ---------------------------------------------------------------------------

/// The raw encoding of the focus instruction(s) for `opcode`.
///
/// Operands are the smallest well-formed values that produce the operand form
/// the generated program produces — `newarray` carries all eight `atype`s,
/// `multianewarray` both dimension counts the recipe emits, `wide` all three
/// prefixed shapes. Every sequence ends with `return` so it walks to exactly its
/// own length: a witness the walker cannot fully decode would silently report as
/// an unscannable method rather than as a failing test.
pub fn witness_bytes(opcode: u8) -> Vec<u8> {
    let mut v = match opcode {
        // tableswitch at pc 0: 3 pad bytes, default, low = 0, high = 1, 2 offsets.
        0xaa => {
            let mut t = vec![0xaa, 0, 0, 0];
            for word in [0i32, 0, 1, 0, 0] {
                t.extend_from_slice(&word.to_be_bytes());
            }
            t
        }
        // lookupswitch at pc 0: 3 pad bytes, default, npairs = 1, one pair.
        0xab => {
            let mut t = vec![0xab, 0, 0, 0];
            for word in [0i32, 1, 7, 0] {
                t.extend_from_slice(&word.to_be_bytes());
            }
            t
        }
        // wide iload 256, wide istore 256, wide iinc 256 by 1000.
        0xc4 => vec![
            0xc4, 0x15, 0x01, 0x00, 0xc4, 0x36, 0x01, 0x00, 0xc4, 0x84, 0x01, 0x00, 0x03, 0xe8,
        ],
        // newarray: every atype the JVMS defines, which is every one the
        // recipe's eight allocations emit.
        0xbc => (4u8..=11).flat_map(|atype| [0xbc, atype]).collect(),
        // multianewarray with the two dimension counts the recipe emits.
        0xc5 => vec![0xc5, 0x00, 0x03, 0x02, 0xc5, 0x00, 0x03, 0x03],
        _ => {
            // Fixed-length: the opcode plus zeroed operands. Zero is a valid
            // shape for every fixed-length operand (a cp index, a local slot, a
            // branch offset, an immediate) as far as the form key is concerned.
            let probe = [opcode, 0, 0, 0, 0, 0, 0, 0];
            let len = matrix::instruction_len(&probe, 0).unwrap_or(1);
            probe[..len].to_vec()
        }
    };
    v.push(0xb1); // return
    v
}

/// The operand forms `witness` decodes to. Derived by walking the bytes with
/// the same decoder the matrix uses, so a form can never be asserted by hand
/// and drift from what the walker actually reports.
pub fn witness_forms(witness: &[u8]) -> Vec<String> {
    let mut forms = Vec::new();
    let mut pc = 0usize;
    while pc < witness.len() {
        let Some(len) = matrix::instruction_len(witness, pc) else {
            break;
        };
        forms.push(matrix::operand_form(witness, pc));
        pc += len;
    }
    forms.dedup();
    forms
}

/// Wrap `code` in a minimal but structurally real class file, so
/// [`matrix::scan_class_bytes`] can be exercised without a JDK.
///
/// Deliberately *not* runnable: it has no usable constant pool beyond the one
/// `Utf8` the scanner needs to find `Code`. Its only job is to be a well-formed
/// carrier for an instruction sequence, which is what makes the walker's
/// per-opcode test hermetic — no `javac`, no subprocess, no temp directory.
/// `handlers` synthesises that many (all-zero) exception-table entries, so the
/// `exception` code-side gate can be exercised too.
pub fn witness_class(code: &[u8], handlers: u16) -> Vec<u8> {
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];
    // constant_pool_count = 2 => one entry, #1 = Utf8 "Code".
    b.extend_from_slice(&2u16.to_be_bytes());
    b.push(1);
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(b"Code");
    // access_flags, this_class, super_class, interfaces_count = 0
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    b.extend_from_slice(&0u16.to_be_bytes()); // fields_count
    b.extend_from_slice(&1u16.to_be_bytes()); // methods_count
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // access, name, descriptor
    b.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    b.extend_from_slice(&1u16.to_be_bytes()); // attribute_name_index = "Code"

    let mut code_attr: Vec<u8> = Vec::new();
    code_attr.extend_from_slice(&8u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&512u16.to_be_bytes()); // max_locals
    code_attr.extend_from_slice(&(code.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(code);
    code_attr.extend_from_slice(&handlers.to_be_bytes());
    for _ in 0..handlers {
        code_attr.extend_from_slice(&[0; 8]);
    }
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

    b.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    b.extend_from_slice(&code_attr);
    b
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum;
    use crate::matrix::scan_class_bytes;

    fn all() -> Vec<OpcodeProgram> {
        generate_all(&OpcodeCorpusConfig::default())
    }

    /// The JVMS 6.5 opcode table, taken from OUTSIDE this crate.
    ///
    /// WHY THIS EXISTS. Until 2026-08-13
    /// `there_is_an_entry_for_every_named_opcode_and_nothing_else` asserted
    /// `p.mnemonic == matrix::opcode_name(p.opcode)` — but `generate_all` builds
    /// the corpus as `(0..=0xc9).filter_map(matrix::opcode_name)`, so the corpus
    /// and the assertion were the same expression evaluated twice. A misspelled
    /// mnemonic (`invokedyanmic`), a swapped `dup_x2`/`dup2_x1`, or any wrong
    /// opcode-to-name mapping in `matrix::NAMES` was invisible: the corpus
    /// carried the wrong name and the test agreed with it. The `len() == 202`
    /// half was already a compile-time fact of `const NAMES: [&str; 202]`.
    /// (E25 sweep,
    /// `docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md`
    /// section 4.1, row 34.)
    ///
    /// PROVENANCE — two independent JDK 25 sources, cross-checked against each
    /// other on 2026-08-13, neither of which is `matrix::opcode_name`:
    ///
    /// 1. `jdk.hotspot.agent/sun/jvm/hotspot/interpreter/Bytecodes.java` from a
    ///    JDK 25 source checkout (`C:\craton\jdk25src` on this host) — the
    ///    Serviceability Agent's bytecode table. Every `def(_x, "x", ...)` row
    ///    joined to its `public static final int _x = N;` value; 202 rows with
    ///    value <= 0xc9. That join is the table below, verbatim.
    /// 2. `java.lang.classfile.Opcode` (JEP 484 ClassFile API), enumerated by
    ///    running `Opcode.values()` on
    ///    `openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS`
    ///    on this host — skipping `isWide()` pseudo-opcodes, keyed by
    ///    `bytecode()`, lowercased. 201 rows.
    ///
    /// The two agree on all 201 opcodes they share. Source 2 has no entry for
    /// `0xc4 wide`, because the ClassFile API models `wide` as a modifier on the
    /// widened opcode rather than as an opcode of its own; source 1 does, JVMS
    /// 6.5 does, and this crate's decoder must, so the table keeps it.
    ///
    /// RESIDUAL: a transcribed table carries the "JDK 26 changes it" risk, and
    /// nothing re-derives this one at test time. The risk is small — the last
    /// assignment added below `0xca` was `0xba invokedynamic` in Java 7 — but it
    /// is not zero. Re-run the two extractions on a JDK bump.
    #[rustfmt::skip]
    const JVMS_MNEMONICS: &[(u8, &str)] = &[
    (0x00, "nop"), (0x01, "aconst_null"), (0x02, "iconst_m1"), (0x03, "iconst_0"),
    (0x04, "iconst_1"), (0x05, "iconst_2"), (0x06, "iconst_3"), (0x07, "iconst_4"),
    (0x08, "iconst_5"), (0x09, "lconst_0"), (0x0a, "lconst_1"), (0x0b, "fconst_0"),
    (0x0c, "fconst_1"), (0x0d, "fconst_2"), (0x0e, "dconst_0"), (0x0f, "dconst_1"),
    (0x10, "bipush"), (0x11, "sipush"), (0x12, "ldc"), (0x13, "ldc_w"), (0x14, "ldc2_w"),
    (0x15, "iload"), (0x16, "lload"), (0x17, "fload"), (0x18, "dload"), (0x19, "aload"),
    (0x1a, "iload_0"), (0x1b, "iload_1"), (0x1c, "iload_2"), (0x1d, "iload_3"),
    (0x1e, "lload_0"), (0x1f, "lload_1"), (0x20, "lload_2"), (0x21, "lload_3"),
    (0x22, "fload_0"), (0x23, "fload_1"), (0x24, "fload_2"), (0x25, "fload_3"),
    (0x26, "dload_0"), (0x27, "dload_1"), (0x28, "dload_2"), (0x29, "dload_3"),
    (0x2a, "aload_0"), (0x2b, "aload_1"), (0x2c, "aload_2"), (0x2d, "aload_3"),
    (0x2e, "iaload"), (0x2f, "laload"), (0x30, "faload"), (0x31, "daload"), (0x32, "aaload"),
    (0x33, "baload"), (0x34, "caload"), (0x35, "saload"), (0x36, "istore"), (0x37, "lstore"),
    (0x38, "fstore"), (0x39, "dstore"), (0x3a, "astore"), (0x3b, "istore_0"),
    (0x3c, "istore_1"), (0x3d, "istore_2"), (0x3e, "istore_3"), (0x3f, "lstore_0"),
    (0x40, "lstore_1"), (0x41, "lstore_2"), (0x42, "lstore_3"), (0x43, "fstore_0"),
    (0x44, "fstore_1"), (0x45, "fstore_2"), (0x46, "fstore_3"), (0x47, "dstore_0"),
    (0x48, "dstore_1"), (0x49, "dstore_2"), (0x4a, "dstore_3"), (0x4b, "astore_0"),
    (0x4c, "astore_1"), (0x4d, "astore_2"), (0x4e, "astore_3"), (0x4f, "iastore"),
    (0x50, "lastore"), (0x51, "fastore"), (0x52, "dastore"), (0x53, "aastore"),
    (0x54, "bastore"), (0x55, "castore"), (0x56, "sastore"), (0x57, "pop"), (0x58, "pop2"),
    (0x59, "dup"), (0x5a, "dup_x1"), (0x5b, "dup_x2"), (0x5c, "dup2"), (0x5d, "dup2_x1"),
    (0x5e, "dup2_x2"), (0x5f, "swap"), (0x60, "iadd"), (0x61, "ladd"), (0x62, "fadd"),
    (0x63, "dadd"), (0x64, "isub"), (0x65, "lsub"), (0x66, "fsub"), (0x67, "dsub"),
    (0x68, "imul"), (0x69, "lmul"), (0x6a, "fmul"), (0x6b, "dmul"), (0x6c, "idiv"),
    (0x6d, "ldiv"), (0x6e, "fdiv"), (0x6f, "ddiv"), (0x70, "irem"), (0x71, "lrem"),
    (0x72, "frem"), (0x73, "drem"), (0x74, "ineg"), (0x75, "lneg"), (0x76, "fneg"),
    (0x77, "dneg"), (0x78, "ishl"), (0x79, "lshl"), (0x7a, "ishr"), (0x7b, "lshr"),
    (0x7c, "iushr"), (0x7d, "lushr"), (0x7e, "iand"), (0x7f, "land"), (0x80, "ior"),
    (0x81, "lor"), (0x82, "ixor"), (0x83, "lxor"), (0x84, "iinc"), (0x85, "i2l"),
    (0x86, "i2f"), (0x87, "i2d"), (0x88, "l2i"), (0x89, "l2f"), (0x8a, "l2d"), (0x8b, "f2i"),
    (0x8c, "f2l"), (0x8d, "f2d"), (0x8e, "d2i"), (0x8f, "d2l"), (0x90, "d2f"), (0x91, "i2b"),
    (0x92, "i2c"), (0x93, "i2s"), (0x94, "lcmp"), (0x95, "fcmpl"), (0x96, "fcmpg"),
    (0x97, "dcmpl"), (0x98, "dcmpg"), (0x99, "ifeq"), (0x9a, "ifne"), (0x9b, "iflt"),
    (0x9c, "ifge"), (0x9d, "ifgt"), (0x9e, "ifle"), (0x9f, "if_icmpeq"), (0xa0, "if_icmpne"),
    (0xa1, "if_icmplt"), (0xa2, "if_icmpge"), (0xa3, "if_icmpgt"), (0xa4, "if_icmple"),
    (0xa5, "if_acmpeq"), (0xa6, "if_acmpne"), (0xa7, "goto"), (0xa8, "jsr"), (0xa9, "ret"),
    (0xaa, "tableswitch"), (0xab, "lookupswitch"), (0xac, "ireturn"), (0xad, "lreturn"),
    (0xae, "freturn"), (0xaf, "dreturn"), (0xb0, "areturn"), (0xb1, "return"),
    (0xb2, "getstatic"), (0xb3, "putstatic"), (0xb4, "getfield"), (0xb5, "putfield"),
    (0xb6, "invokevirtual"), (0xb7, "invokespecial"), (0xb8, "invokestatic"),
    (0xb9, "invokeinterface"), (0xba, "invokedynamic"), (0xbb, "new"), (0xbc, "newarray"),
    (0xbd, "anewarray"), (0xbe, "arraylength"), (0xbf, "athrow"), (0xc0, "checkcast"),
    (0xc1, "instanceof"), (0xc2, "monitorenter"), (0xc3, "monitorexit"), (0xc4, "wide"),
    (0xc5, "multianewarray"), (0xc6, "ifnull"), (0xc7, "ifnonnull"), (0xc8, "goto_w"),
    (0xc9, "jsr_w"),
    ];

    /// `matrix::opcode_name` answers the JVMS, not itself.
    ///
    /// This is the test that makes a typo in `matrix::NAMES` reportable. It
    /// compares the whole `u8` domain, not the corpus's own range, so a name
    /// appearing where the JVMS has a reserved opcode is caught too.
    #[test]
    fn the_matrix_opcode_table_is_the_jvms_opcode_table() {
        assert_eq!(
            JVMS_MNEMONICS.len(),
            202,
            "the external table lost rows in transcription"
        );
        for (i, (op, _)) in JVMS_MNEMONICS.iter().enumerate() {
            assert_eq!(
                *op as usize, i,
                "the external table is not dense and in opcode order at index {i}"
            );
        }
        let mut wrong: Vec<String> = Vec::new();
        for op in 0u8..=0xff {
            let jvms = JVMS_MNEMONICS
                .iter()
                .find(|(o, _)| *o == op)
                .map(|(_, name)| *name);
            let ours = matrix::opcode_name(op);
            if ours != jvms {
                wrong.push(format!("{op:#04x}: matrix={ours:?} JVMS={jvms:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "matrix::opcode_name disagrees with the JDK 25 opcode table:\n  {}\n\n\
             The JVMS table above came from the Serviceability Agent's \
             Bytecodes.java and from java.lang.classfile.Opcode; if a JDK bump \
             genuinely changed an assignment, re-derive it (see the const's \
             PROVENANCE) rather than editing a row to match this crate.",
            wrong.join("\n  ")
        );
    }

    #[test]
    fn there_is_an_entry_for_every_named_opcode_and_nothing_else() {
        let programs = all();
        // The expectation is the EXTERNAL table's population, not this crate's.
        let named_in_range = JVMS_MNEMONICS.iter().filter(|(op, _)| *op <= 0xc9).count();
        assert_eq!(
            programs.len(),
            named_in_range,
            "one entry per JVMS mnemonic in 0x00..=0xc9"
        );
        for (idx, p) in programs.iter().enumerate() {
            assert_eq!(p.opcode as usize, idx, "entries are in opcode order");
            let jvms = JVMS_MNEMONICS
                .iter()
                .find(|(o, _)| *o == p.opcode)
                .map(|(_, name)| *name);
            assert_eq!(
                Some(p.mnemonic),
                jvms,
                "corpus entry {idx} carries a mnemonic the JVMS does not give \
                 opcode {:#04x}. Before 2026-08-13 this compared against \
                 matrix::opcode_name, which is where the corpus got the name \
                 from, so it could not fail.",
                p.opcode
            );
        }
    }

    #[test]
    fn the_generator_emits_a_program_per_opcode_and_the_walker_rediscovers_it() {
        // The load-bearing test: for every opcode this build claims to
        // generate, the matrix walker finds *that* opcode in the program's
        // witness. Hermetic on purpose — no javac, so a CI worker without a JDK
        // still fails the day a recipe and the walker disagree.
        let mut generated = 0usize;
        for p in all() {
            if !p.is_generated() {
                continue;
            }
            generated += 1;
            let class = witness_class(&p.witness, 1);
            let scan = scan_class_bytes(&p.class_name, &class)
                .unwrap_or_else(|| panic!("{} witness is not a walkable class", p.class_name));
            assert!(
                scan.sites.keys().any(|k| k
                    .split_once(':')
                    .and_then(|(op, _)| op.parse::<u8>().ok())
                    == Some(p.opcode)),
                "{} ({}): the walker did not rediscover opcode {:#04x} in its witness; sites = {:?}",
                p.class_name,
                p.mnemonic,
                p.opcode,
                scan.sites.keys().collect::<Vec<_>>()
            );
        }
        assert_eq!(
            generated,
            202 - 5,
            "five opcodes are unreachable from Java source; everything else is generated"
        );
    }

    #[test]
    fn every_witness_walks_to_exactly_its_own_length() {
        // A witness the walker can only partially decode would make the test
        // above pass for the wrong reason (the focus opcode is first).
        for p in all() {
            let mut pc = 0usize;
            while pc < p.witness.len() {
                let len = matrix::instruction_len(&p.witness, pc).unwrap_or_else(|| {
                    panic!("{}: undecodable at pc {pc}", p.class_name);
                });
                pc += len;
            }
            assert_eq!(pc, p.witness.len(), "{}: overran its witness", p.class_name);
            assert!(!p.forms.is_empty(), "{}: no derived form", p.class_name);
        }
    }

    #[test]
    fn the_operand_forms_the_recipes_target_are_present() {
        let by_op: std::collections::BTreeMap<u8, OpcodeProgram> =
            all().into_iter().map(|p| (p.opcode, p)).collect();
        // newarray covers every atype: a boolean[] and a double[] are different
        // paths in all four executors, so one `newarray` row would be a lie.
        let newarray = &by_op[&0xbc].forms;
        for ty in [
            "boolean", "char", "float", "double", "byte", "short", "int", "long",
        ] {
            assert!(
                newarray.contains(&format!("atype:{ty}")),
                "newarray witness is missing atype:{ty}"
            );
        }
        assert!(by_op[&0xc4].forms.contains(&"wide:iinc".to_string()));
        assert!(by_op[&0xc4].forms.contains(&"wide:iload".to_string()));
        assert!(by_op[&0xc5].forms.contains(&"cp16,dims:3".to_string()));
        assert!(by_op[&0xaa].forms.contains(&"table".to_string()));
        assert!(by_op[&0xab].forms.contains(&"lookup".to_string()));
        assert!(by_op[&0xc8].forms.contains(&"branch:s32".to_string()));
    }

    #[test]
    fn every_generated_program_declares_a_checksum() {
        for p in all() {
            let Some(source) = &p.source else { continue };
            assert!(
                source.contains(&format!("{} {} ", checksum::MARKER, p.checksum_name)),
                "{} declares no checksum under {}",
                p.class_name,
                p.checksum_name
            );
            // And the name must survive the extractor. A marker whose name is
            // empty or whitespace is dropped silently by `checksum::extract`,
            // which would leave the dimension exactly as vacuous as having no
            // marker at all — the failure this corpus exists to end.
            let emitted = format!("{} {} deadbeefdeadbeef", checksum::MARKER, p.checksum_name);
            assert_eq!(
                checksum::extract(&emitted)
                    .get(&p.checksum_name)
                    .map(String::as_str),
                Some("deadbeefdeadbeef"),
                "{}: the declared checksum name does not round-trip",
                p.class_name
            );
        }
    }

    #[test]
    fn generation_is_deterministic() {
        let config = OpcodeCorpusConfig::default();
        assert_eq!(generate_all(&config), generate_all(&config));
        // And sensitive to its inputs, so the config is really an input rather
        // than decoration.
        let other = OpcodeCorpusConfig {
            loop_iterations: 7,
            ..OpcodeCorpusConfig::default()
        };
        assert_ne!(generate_all(&config), generate_all(&other));
    }

    #[test]
    fn generated_sources_are_well_formed_and_self_naming() {
        for p in all() {
            let Some(source) = &p.source else { continue };
            assert!(
                source.contains(&format!("public class {}", p.class_name)),
                "{}: class declaration does not match the file stem",
                p.class_name
            );
            assert!(source.contains("public static void main(String[] args)"));
            assert!(
                !source.contains('@'),
                "{}: an unreplaced template placeholder survived",
                p.class_name
            );
            assert!(balanced(source), "{}: unbalanced braces", p.class_name);
            assert!(
                source.starts_with("// difftest: strict"),
                "{}: missing the strict pragma header",
                p.class_name
            );
        }
    }

    /// The opcodes whose focus instruction cannot sit in `main` — it needs a
    /// parameter pinned to a particular slot, a particular return type, more
    /// locals than a byte can index, or a method body too large to branch
    /// across. For those the loop and the handler have to be *inside the
    /// helper*, because [`matrix::SiteFacts`] do not cross a method boundary.
    const HELPER_FOCUSED: &[u8] = &[
        0x13, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22,
        0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x36, 0x37, 0x38, 0x39,
        0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
        0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xc4, 0xc8,
    ];

    #[test]
    fn programs_wrap_their_focus_in_a_loop_and_a_handler() {
        // The `osr` and `exception` code sides are per method. If this ever
        // stops holding, those two columns quietly empty out while the opcode
        // count keeps rising, which is exactly the misleading number the matrix
        // exists to prevent.
        for p in all() {
            let Some(source) = &p.source else { continue };
            assert!(
                source.contains("for (int i = 0; i < trips; i++)"),
                "{}: main has no back-edge",
                p.class_name
            );
            assert!(
                source.contains("catch (Throwable caught)"),
                "{}: main has no handler",
                p.class_name
            );
            if HELPER_FOCUSED.contains(&p.opcode) {
                assert!(
                    source.contains("for (int k = 0; k < 3; k++)"),
                    "{}: the focus is in a helper, which has no back-edge of its own",
                    p.class_name
                );
                assert!(
                    source.contains("catch (RuntimeException ex)"),
                    "{}: the focus is in a helper, which has no handler of its own",
                    p.class_name
                );
            }
        }
    }

    #[test]
    fn the_helper_builder_always_emits_a_loop_and_a_handler() {
        let h = helper("h", "int p", "int a = p;", "s += a;");
        assert!(h.contains("for (int k = 0; k < 3; k++)"));
        assert!(h.contains("catch (RuntimeException ex)"));
        assert!(
            h.contains("    int a = p;"),
            "decls are indented into place"
        );
        assert!(balanced(&h), "helper braces: {h}");
    }

    #[test]
    fn the_five_unreachable_opcodes_say_why() {
        let by_op: std::collections::BTreeMap<u8, OpcodeProgram> =
            all().into_iter().map(|p| (p.opcode, p)).collect();
        for op in [0x00u8, 0x5f, 0xa8, 0xa9, 0xc9] {
            let p = &by_op[&op];
            assert!(p.source.is_none(), "{} must not be generated", p.mnemonic);
            let reason = p
                .support
                .reason()
                .unwrap_or_else(|| panic!("{} has no reason", p.mnemonic));
            assert!(
                reason.len() > 40,
                "{}: reason is not actionable",
                p.mnemonic
            );
        }
        // Everything else is generated.
        for p in by_op.values() {
            if matches!(p.opcode, 0x00 | 0x5f | 0xa8 | 0xa9 | 0xc9) {
                continue;
            }
            assert!(p.is_generated(), "{} should be generated", p.mnemonic);
        }
    }

    #[test]
    fn the_large_recipes_are_sized_by_config_and_can_be_skipped() {
        let big = OpcodeCorpusConfig::default();
        let by_op: std::collections::BTreeMap<u8, OpcodeProgram> = generate_all(&big)
            .into_iter()
            .map(|p| (p.opcode, p))
            .collect();
        // goto_w needs a >32 KiB method: ~4 bytes per statement.
        let goto_w = by_op[&0xc8].source.as_ref().expect("goto_w generated");
        assert!(goto_w.matches("s += 1;").count() >= 9_000);
        // wide needs >255 locals.
        let wide = by_op[&0xc4].source.as_ref().expect("wide generated");
        assert!(wide.contains("int v299 = p + 299;"));
        assert!(wide.contains("v299 += 1000;"));

        let small = OpcodeCorpusConfig {
            include_large: false,
            ..OpcodeCorpusConfig::default()
        };
        let skipped: Vec<u8> = generate_all(&small)
            .into_iter()
            .filter(|p| matches!(p.support, OpcodeSupport::Skipped(_)))
            .map(|p| p.opcode)
            .collect();
        assert_eq!(skipped, vec![0xc4, 0xc8]);
    }

    #[test]
    fn the_generator_index_carries_reasons_into_the_matrix() {
        let index = generator_index(&OpcodeCorpusConfig::default());
        assert_eq!(index.len(), 202);
        assert_eq!(index[&0x60], GeneratorStatus::Generated);
        match &index[&0xa8] {
            GeneratorStatus::Unreachable { reason } => assert!(reason.contains("jsr")),
            other => panic!("jsr must be unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_witness_class_reports_the_handler_and_loop_facts_the_walker_needs() {
        // `witness_class(.., 1)` must give the scanned sites a handler, and a
        // backward branch must give them a loop — the two code-side gates.
        let scan = scan_class_bytes("W", &witness_class(&[0x60, 0xb1], 1)).expect("walkable");
        assert!(scan.sites.values().all(|f| f.in_exception_method));
        let looped = scan_class_bytes("L", &witness_class(&[0x1a, 0x9a, 0xff, 0xfe, 0xb1], 0))
            .expect("walkable");
        assert!(looped.sites.values().all(|f| f.in_loop_method));
    }

    /// Brace balance over generated Java, ignoring comment lines and the
    /// contents of string and char literals. Comment lines are dropped whole
    /// because an apostrophe in prose ("javac's") would otherwise open a char
    /// literal that never closes and silently disable the rest of the check.
    fn balanced(s: &str) -> bool {
        let mut depth = 0i32;
        for line in s.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut in_string = false;
            let mut in_char = false;
            let mut prev = '\0';
            for ch in line.chars() {
                match ch {
                    '"' if !in_char && prev != '\\' => in_string = !in_string,
                    '\'' if !in_string && prev != '\\' => in_char = !in_char,
                    '{' if !in_string && !in_char => depth += 1,
                    '}' if !in_string && !in_char => depth -= 1,
                    _ => {}
                }
                if depth < 0 {
                    return false;
                }
                prev = ch;
            }
        }
        depth == 0
    }
}
