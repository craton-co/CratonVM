// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

impl Compiler {
    fn c1_branch_cells(&self, pc: usize) -> Option<(usize, usize)> {
        if !self.inline_scope_stack.is_empty()
            || self
                .synthetic_guard_span
                .is_some_and(|(lo, hi)| pc >= lo && pc < hi)
        {
            return None;
        }
        self.c1_branch_counters
            .as_ref()?
            .native_branch_cells(self.orig_bci(pc))
    }

    pub(super) fn emit_c1_branch_profile(&mut self, pc: usize, cc: u8) {
        self.emit_c1_branch_profile_condition(pc, cc, None);
    }

    /// A fused FP equality test also depends on parity: unordered is unequal.
    pub(super) fn emit_c1_branch_profile_condition(
        &mut self,
        pc: usize,
        cc: u8,
        unordered: Option<bool>,
    ) {
        let Some((taken, not_taken)) = self.c1_branch_cells(pc) else {
            return;
        };
        // R10/R11 are reserved scratch, outside both operand-stack pools.
        // MOV and CMOV consume the original flags without changing them.
        self.emit_mov_imm64(R10, not_taken as i64);
        self.emit_mov_imm64(R11, taken as i64);
        self.buf.emit(&[0x4d, 0x0f, 0x40 | (cc & 15), 0xd3]); // CMOVcc R10,R11
        if let Some(branch) = unordered {
            self.emit_mov_imm64(R11, if branch { taken } else { not_taken } as i64);
            self.buf.emit(&[0x4d, 0x0f, 0x4a, 0xd3]); // CMOVP R10,R11
        }
        self.emit_c1_branch_increment();
    }

    pub(super) fn emit_c1_branch_profile_known(&mut self, pc: usize, branch: bool) {
        let Some((taken, not_taken)) = self.c1_branch_cells(pc) else {
            return;
        };
        self.emit_mov_imm64(R10, if branch { taken } else { not_taken } as i64);
        self.emit_c1_branch_increment();
    }

    fn emit_c1_branch_increment(&mut self) {
        // Borrow the first 16 bytes of the frame's unconditional 32-byte
        // outgoing area. RSP never goes below its checked frame floor; no
        // call or safepoint occurs before RAX, flags and RSP are restored.
        self.buf.emit(&[0x48, 0x8d, 0x64, 0x24, 0x10]); // LEA RSP,[RSP+16]
        self.buf.emit(&[0x9c, 0x50]); // PUSHFQ; PUSH RAX
        self.buf.emit(&[0x41, 0x8b, 0x02]); // MOV EAX,[R10]
        let retry = self.buf.pos();
        self.buf.emit(&[0x83, 0xf8, 0xff]); // CMP EAX,MAX
        self.buf.emit(&[0x0f, 0x84]);
        let done = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        self.buf.emit(&[0x44, 0x8d, 0x58, 0x01]); // LEA R11D,[RAX+1]
        self.buf.emit(&[0xf0, 0x45, 0x0f, 0xb1, 0x1a]); // LOCK CMPXCHG [R10],R11D
        self.buf.emit(&[0x0f, 0x85]);
        let back = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // Cast: this backward distance contains only the fixed-size CAS loop.
        let rel = -((back + 4 - retry) as i32);
        self.buf.try_patch_i32(back, rel).ok();
        self.patch_rel32_to_here(done);
        self.buf.emit(&[0x58, 0x9d]); // POP RAX; POPFQ
        self.buf.emit(&[0x48, 0x8d, 0x64, 0x24, 0xf0]); // LEA RSP,[RSP-16]

        // The conservative frame sweep includes outgoing words. Do not leave
        // an otherwise dead reference copied here after restoring RAX.
        self.buf.emit(&[0x48, 0xc7, 0x04, 0x24, 0, 0, 0, 0]); // MOV qword [RSP],0
    }
}
