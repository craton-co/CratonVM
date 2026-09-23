// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end regressions for the two JIT defects behind H2
//! `org.h2.test.db.TestBigResult`, which failed with a garbage `int` in three
//! different places depending on which one a run reached first:
//! `ArrayIndexOutOfBoundsException: arraycopy: destination index -535859552`
//! inside `HeapCharBuffer.compact`, `NegativeArraySizeException: nbits < 0:
//! -702434216`, and a disk-spilled result set with the wrong number of rows.
//!
//! Both put the same kind of value where an `int` belonged: the bits of
//! something else that happened to sit in the word the compiled code read.
//!
//!   * **Guarded-inline join (single-pass tier).** A receiver-guarded virtual
//!     inline emits `CMP class; JNE miss; <spliced body>; JMP done`, the normal
//!     dispatch as the miss path, and compiles everything after `done` against
//!     the DISPATCH path's operand-stack model. The spliced body put its result
//!     at the lowest popped argument slot; the dispatch path at its own
//!     canonical slot. They differ whenever `flush_scratch_registers` handed a
//!     register-resident operand a slot above a shallower one, which
//!     `aload_0; iconst_0; invokevirtual ix(I)I` with `this` in a callee-saved
//!     register does. The code after the join then read the RECEIVER's pointer
//!     bits as the call's result. `HeapCharBuffer.compact` is
//!     `System.arraycopy(hb, ix(pos), hb, ix(0), rem)` — exactly that shape.
//!
//!   * **Entry trap, register-resident parameter (optimizing tier).** An IR
//!     body that traps before the scheduler has emitted a parameter's
//!     `Op::Param` node described that parameter by the callee-saved register
//!     linear scan reserved for it as a loop-carried value — a register still
//!     holding the CALLER's value. Called from a compiled caller, the precise
//!     resume rebuilt the parameter from it. `testSortingAndDistinct2` traps at
//!     bci 2 on its `"SET MAX_MEMORY_ROWS " + maxRows` concat and then runs
//!     `new BitSet(partCount)`.
//!
//! Each probe asserts the observable Java result AND that the path it exists
//! for actually ran, because both defects depend on tiering decisions that a
//! future heuristic change could route around, turning the probe into a test
//! of nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const JOIN_SRC: &str = r#"
public class JitGuardedJoinProbe {
    static int seen = -12345;
    static void s1(int d) { seen = d; }

    static class Heap {
        final char[] hb;
        final int offset;
        int position, limit, capacity;
        Heap(int cap, int off) { hb = new char[cap + off]; offset = off; capacity = cap; limit = cap; }
        protected int ix(int i) { return i + offset; }
        // The call result consumed straight from its operand-stack slot.
        int passToCall() { s1(ix(0)); return seen; }
        // HeapCharBuffer.compact, verbatim in shape.
        void compact() {
            int pos = position, lim = limit;
            int rem = (pos <= lim ? lim - pos : 0);
            System.arraycopy(hb, ix(pos), hb, ix(0), rem);
            position = rem;
            limit = capacity;
        }
    }

    public static void main(String[] args) {
        Heap h = new Heap(64, 3);
        for (int i = 0; i < h.hb.length; i++) h.hb[i] = (char) i;
        String bad = null;
        for (int r = 0; r < 200000 && bad == null; r++) {
            seen = -1;
            int got = h.passToCall();
            if (got != 3) bad = "passToCall r=" + r + " got=" + got;
        }
        System.out.println("passToCall=" + (bad == null ? "ok" : bad));
        String cbad = null;
        for (int r = 0; r < 200000 && cbad == null; r++) {
            h.position = 10;
            h.limit = h.capacity;
            h.hb[3 + 10] = (char) (r & 0x7fff);
            try {
                h.compact();
            } catch (Throwable t) {
                cbad = "compact r=" + r + " " + t;
                break;
            }
            if (h.hb[3] != (char) (r & 0x7fff) || h.position != 54) {
                cbad = "compact r=" + r + " hb[off]=" + (int) h.hb[3] + " position=" + h.position;
            }
        }
        System.out.println("compact=" + (cbad == null ? "ok" : cbad));
        System.out.println("PROBE_DONE");
    }
}
"#;

const ENTRY_TRAP_SRC: &str = r#"
import java.util.BitSet;

public class JitEntryTrapParamProbe {
    interface Stmt { boolean execute(String sql); Rs executeQuery(String sql); }
    interface Rs { boolean next(); int getInt(int col); }
    static final class MemRs implements Rs {
        final int partCount; int row;
        MemRs(int partCount) { this.partCount = partCount; }
        public boolean next() { return ++row <= partCount * 10; }
        public int getInt(int col) {
            int r = row - 1;
            return col == 1 ? r / partCount + 1 : partCount - r % partCount;
        }
    }
    static final class MemStmt implements Stmt {
        int partCount; String lastSql;
        public boolean execute(String sql) { lastSql = sql; return false; }
        public Rs executeQuery(String sql) { return new MemRs(partCount); }
    }
    void assertTrue(boolean b) { if (!b) throw new AssertionError("expected true"); }
    void assertFalse(boolean b) { if (b) throw new AssertionError("expected false"); }
    void assertEquals(int a, int b) { if (a != b) throw new AssertionError("expected " + a + " got " + b); }

    // TestBigResult.testSortingAndDistinct2, statement for statement. The
    // concat at bci 2 is what the optimizing tier compiles as a trap.
    private void sortingAndDistinct(Stmt stat, String sql, int maxRows, int partCount) {
        Rs rs;
        stat.execute("SET MAX_MEMORY_ROWS " + maxRows);
        rs = stat.executeQuery(sql);
        BitSet set = new BitSet(partCount);
        for (int i = 1; i <= 10; i++) {
            set.clear();
            for (int j = 1; j <= partCount; j++) {
                assertTrue(rs.next());
                assertEquals(i, rs.getInt(1));
                set.set(rs.getInt(2));
            }
            assertEquals(partCount + 1, set.nextClearBit(1));
        }
        assertFalse(rs.next());
    }

    // The caller is compiled (a hot loop OSRs it) before it makes the calls;
    // the defect needs a compiled caller's registers under the callee.
    int run(MemStmt st) {
        long warm = 0;
        for (int i = 0; i < 300000; i++) warm += st.execute("x") ? 1 : i & 3;
        for (int r = 0; r < 8; r++) {
            int pc = (r & 1) == 0 ? 100 : 400;
            st.partCount = pc;
            sortingAndDistinct(st, "SELECT", (r & 1) == 0 ? 1000 : 10, pc);
        }
        return (int) warm;
    }

    public static void main(String[] args) {
        JitEntryTrapParamProbe t = new JitEntryTrapParamProbe();
        MemStmt st = new MemStmt();
        String bad = null;
        for (int k = 0; k < 6 && bad == null; k++) {
            try {
                t.run(st);
            } catch (Throwable e) {
                bad = "k=" + k + " " + e;
            }
        }
        System.out.println("entryTrap=" + (bad == null ? "ok" : bad));
        System.out.println("PROBE_DONE");
    }
}
"#;

fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Compile `src` as `class_name` into its own temp directory.
fn compile_probe(jdk: &Path, class_name: &str, src: &str) -> Option<PathBuf> {
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let dir = std::env::temp_dir().join(format!("cratonvm-{class_name}"));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{class_name}.java"));
    let _ = std::fs::remove_file(dir.join(format!("{class_name}.class")));
    std::fs::write(&file, src).expect("write probe source");
    let out = match Command::new(&javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&file)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[{class_name}] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join(format!("{class_name}.class")).exists(),
        "[{class_name}] the embedded probe failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(
    bin: &Path,
    jdk: &Path,
    classes: &Path,
    class_name: &str,
    env: &[(&str, &str)],
) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home")
        .arg(jdk)
        .arg("-c")
        .arg(classes)
        .arg(class_name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn cratonvm");
    // Drained, not polled: the diagnostic flags below make stderr far larger
    // than a pipe buffer, and a child blocked on a full pipe reads as a hang.
    let done = common::wait_draining(child, Duration::from_secs(240));
    let stdout = String::from_utf8_lossy(&done.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&done.output.stderr).into_owned();
    assert!(
        !done.timed_out,
        "[{class_name}] probe timed out.
stdout:
{stdout}
stderr tail:
{}",
        tail(&stderr)
    );
    (stdout, stderr)
}

fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}

/// Tail of a long stderr, for assertion messages.
fn tail(s: &str) -> &str {
    let cut = s.len().saturating_sub(4000);
    let mut i = cut;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn setup(class_name: &str, src: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[{class_name}] cratonvm binary not found (set CRATONVM_BIN); skipping");
        return None;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[{class_name}] no JDK found (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
        return None;
    };
    let classes = compile_probe(&jdk, class_name, src)?;
    Some((bin, jdk, classes))
}

#[test]
fn a_guarded_inline_hands_its_result_to_the_code_after_the_join() {
    let Some((bin, jdk, classes)) = setup("JitGuardedJoinProbe", JOIN_SRC) else {
        return;
    };
    // The join reconciliation names itself under JITC.
    let (stdout, stderr) = run_probe(
        &bin,
        &jdk,
        &classes,
        "JitGuardedJoinProbe",
        &[("CRATONVM_DBG_JITC", "1")],
    );
    assert!(
        stdout.contains("PROBE_DONE"),
        "probe did not finish.\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );
    assert_eq!(
        field(&stdout, "passToCall=").as_deref(),
        Some("ok"),
        "`s1(ix(0))` received something other than `ix(0)`: the spliced body left its \
         result in a different slot from the one the code after the guard join reads.\n\
         stdout:\n{stdout}"
    );
    assert_eq!(
        field(&stdout, "compact=").as_deref(),
        Some("ok"),
        "the `HeapCharBuffer.compact` shape passed a wrong `destPos` to arraycopy.\n\
         stdout:\n{stdout}"
    );
    // Anti-vacuity: the reconciliation path must actually have been taken, or
    // this test proves only that the inline did not happen.
    assert!(
        stderr.contains("guarded-inline join at pc="),
        "no guarded-inline join was reconciled, so the defect's path never ran and this \
         test is vacuous.\nstderr tail:\n{}",
        tail(&stderr)
    );
}

#[test]
fn a_parameter_is_rebuilt_from_its_own_value_when_the_ir_body_traps_at_entry() {
    let Some((bin, jdk, classes)) = setup("JitEntryTrapParamProbe", ENTRY_TRAP_SRC) else {
        return;
    };
    // The IR publish names itself under ir-compiles, the resume under DEOPT.
    let (stdout, stderr) = run_probe(
        &bin,
        &jdk,
        &classes,
        "JitEntryTrapParamProbe",
        &[("CRATONVM_DBG", "ir-compiles"), ("CRATONVM_DBG_DEOPT", "1")],
    );
    assert!(
        stdout.contains("PROBE_DONE"),
        "probe did not finish.\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );
    assert_eq!(
        field(&stdout, "entryTrap=").as_deref(),
        Some("ok"),
        "the precise resume of an entry trap rebuilt `partCount` from a register the \
         caller owned.\nstdout:\n{stdout}"
    );
    // Anti-vacuity, both halves: the method took the optimizing tier, and it
    // was resumed through the dispatch helper from a compiled caller.
    let method = "JitEntryTrapParamProbe.sortingAndDistinct";
    assert!(
        stderr.contains(&format!(
            "[ir] optimizing backend produced a body for {method}"
        )),
        "{method} never got an IR body, so the entry trap never existed.\nstderr tail:\n{}",
        tail(&stderr)
    );
    assert!(
        stderr.contains(&format!("helper precise-resume of trapped callee {method}")),
        "{method}'s trap was never resumed by the dispatch helper, which is the path \
         that read the caller's register.\nstderr tail:\n{}",
        tail(&stderr)
    );
}
