

pub const ROW_BYTES: usize = 64;
pub const ROW_SHIFT: u8 = 6;
pub const ROW_WORDS: usize = ROW_BYTES / 4;
pub const ROWS_CAP: u32 = 1 << 16;
pub const ROWS_BYTES: usize = ROW_BYTES * (ROWS_CAP as usize);

pub const STATUS_PUBLISHED: u32 = 1;
pub const STATUS_HIERARCHY_ONLY: u32 = 2;
pub const PRIMARY_MAX_DEPTH: u32 = 7;
pub const STATUS_DEPTH_SHIFT: u32 = 8;
pub const STATUS_MASK: u32 = 0xFF;
pub const BLOOM_WORD: usize = 8;
pub const SLOT_WORD: usize = 10;
pub const SLOTS: u32 = 6;

const _: () = assert!(1usize << ROW_SHIFT == ROW_BYTES);
const _: () = assert!(SLOT_WORD + SLOTS as usize == ROW_WORDS);
const _: () = assert!((PRIMARY_MAX_DEPTH as usize) < BLOOM_WORD);
const _: () = assert!(ROW_BYTES <= 127);
const _: () = assert!((ROWS_CAP as u64) << ROW_SHIFT <= u32::MAX as u64);

pub const fn iface_slot_word(iface: u32) -> usize {
    SLOT_WORD + (iface % SLOTS) as usize // Cast: < SLOTS
}

pub const fn bloom_bit(iface: u32) -> u32 {
    iface % 64
}

pub fn row_words(chain: &[u32], interfaces: &[u32], hierarchy_only: bool) -> [u32; ROW_WORDS] {
    let mut w = [0u32; ROW_WORDS];
    w[0] = if hierarchy_only {
        STATUS_HIERARCHY_ONLY
    } else {
        STATUS_PUBLISHED
    };
    let own_depth = chain.len().saturating_sub(1) as u32;
    if (1..=PRIMARY_MAX_DEPTH).contains(&own_depth) {
        w[0] |= own_depth << STATUS_DEPTH_SHIFT;
    }
    for d in 1..=PRIMARY_MAX_DEPTH as usize {
        w[d] = chain.get(d).copied().unwrap_or(0);
    }
    let mut bloom = 0u64;
    for &i in interfaces {
        bloom |= 1u64 << bloom_bit(i);
        let slot = iface_slot_word(i);
        if w[slot] == 0 {
            w[slot] = i;
        }
    }
    w[BLOOM_WORD] = bloom as u32;
    w[BLOOM_WORD + 1] = (bloom >> 32) as u32;
    w
}

pub fn typecheck_rows_enabled() -> bool {
    true
}

pub fn typecheck_rows_all_enabled() -> bool {
    false}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowTarget {
    Class { depth: u32 },
    Interface,
}

#[derive(Debug)]
pub(crate) struct RowArm {
    pub(crate) bytes: Vec<u8>,
    pub(crate) hit: Vec<usize>,
    pub(crate) miss: Vec<usize>,
}

pub(crate) fn typecheck_row_bytes(
    rows: usize,
    target: u32,
    kind: RowTarget,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    if target == 0 || rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    let word = match kind {
        RowTarget::Class { depth } if (1..=PRIMARY_MAX_DEPTH).contains(&depth) => depth as usize,
        RowTarget::Class { .. } => return None,
        RowTarget::Interface => iface_slot_word(target),
    };
    let disp = (word * 4) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(96);
    let mut fall: Vec<usize> = Vec::new();
    let mut hit: Vec<usize> = Vec::new();
    let mut miss: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x8B, 0x08]); // MOV ECX, [RAX]
    b.extend_from_slice(&[0x81, 0xF9]); // CMP ECX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address
    let cmp_word = |b: &mut Vec<u8>| {
        b.extend_from_slice(&[0x41, 0x81, 0x7C, 0x0B, disp]);
        b.extend_from_slice(&target.to_le_bytes());
    };
    cmp_word(&mut b);
    hit.push(jcc(&mut b, 0x84)); // JE hit
    if let Some(latch) = miss_latch {
        b.extend_from_slice(&[0x41, 0x80, 0x7C, 0x0B, 0x00, STATUS_HIERARCHY_ONLY as u8]);
        fall.push(jcc(&mut b, 0x85)); // JNE fall
        cmp_word(&mut b); // the deciding word, read after the status
        hit.push(jcc(&mut b, 0x84)); // JE hit
        if kind == RowTarget::Interface {
            let bit = bloom_bit(target);
            let byte = (BLOOM_WORD * 4) as u8 + (bit / 8) as u8;
            let mask = 1u8 << (bit % 8);
            b.extend_from_slice(&[0x41, 0xF6, 0x44, 0x0B, byte, mask]);
            fall.push(jcc(&mut b, 0x85)); // JNZ fall: listed, not placed
        }
        if latch != 0 {
            b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
            b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
            b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
            fall.push(jcc(&mut b, 0x85)); // JNE fall
        }
        b.push(0xE9); // JMP miss
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        miss.push(at);
    }
    let end = b.len();
    for at in fall {
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some(RowArm {
        bytes: b,
        hit,
        miss,
    })
}

pub fn typecheck_row_first_enabled() -> bool {
    true
}

pub fn typecheck_row_first_iface_enabled() -> bool {
    true
}

pub fn typecheck_branch_fusion_enabled() -> bool {
    true
}

pub fn typecheck_shared_class_word_enabled() -> bool {
    true
}

pub fn sp_typecheck_row_first_enabled() -> bool {
    true
}

pub(crate) fn sp_row_first_arm(
    rows: Option<usize>,
    interface: bool,
    depth: u32,
    target: u32,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    if !sp_typecheck_row_first_enabled() {
        return None;
    }
    let kind = if interface {
        if !typecheck_row_first_iface_enabled() {
            return None;
        }
        RowTarget::Interface
    } else {
        if !typecheck_row_first_enabled() {
            return None;
        }
        RowTarget::Class { depth }
    };
    typecheck_row_first_bytes(rows?, target, kind, miss_latch)
}

pub(crate) fn typecheck_row_first_bytes(
    rows: usize,
    target: u32,
    kind: RowTarget,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    if target == 0 || rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    let word = match kind {
        RowTarget::Class { depth } if (1..=PRIMARY_MAX_DEPTH).contains(&depth) => depth as usize,
        RowTarget::Class { .. } => return None,
        RowTarget::Interface => iface_slot_word(target),
    };
    let disp = (word * 4) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(80);
    let mut fall: Vec<usize> = Vec::new();
    let mut hit: Vec<usize> = Vec::new();
    let mut miss: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x8B, 0x08]); // MOV ECX, [RAX]
    b.extend_from_slice(&[0x81, 0xF9]); // CMP ECX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address

    b.extend_from_slice(&[0x41, 0x80, 0x7C, 0x0B, 0x00, STATUS_HIERARCHY_ONLY as u8]);
    fall.push(jcc(&mut b, 0x85)); // JNE fall

    b.extend_from_slice(&[0x41, 0x81, 0x7C, 0x0B, disp]);
    b.extend_from_slice(&target.to_le_bytes());
    hit.push(jcc(&mut b, 0x84)); // JE hit
    if let Some(latch) = miss_latch {
        if kind == RowTarget::Interface {
            let bit = bloom_bit(target);
            let byte = (BLOOM_WORD * 4) as u8 + (bit / 8) as u8;
            let mask = 1u8 << (bit % 8);
            b.extend_from_slice(&[0x41, 0xF6, 0x44, 0x0B, byte, mask]);
            fall.push(jcc(&mut b, 0x85)); // JNZ fall: listed, not placed
        }
        if latch != 0 {
            b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
            b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
            b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
            fall.push(jcc(&mut b, 0x85)); // JNE fall
        }
        b.push(0xE9); // JMP miss
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        miss.push(at);
    }
    let end = b.len();
    for at in fall {
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some(RowArm {
        bytes: b,
        hit,
        miss,
    })
}

pub(crate) fn typecheck_row_first_from_word_bytes(
    rows: usize,
    target: u32,
    kind: RowTarget,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    const LOAD_HEADER: [u8; 2] = [0x8B, 0x08];
    let mut arm = typecheck_row_first_bytes(rows, target, kind, miss_latch)?;
    if arm.bytes.get(..LOAD_HEADER.len()) != Some(&LOAD_HEADER[..]) {
        return None;
    }
    arm.bytes.drain(..LOAD_HEADER.len());
    for edge in arm.hit.iter_mut().chain(arm.miss.iter_mut()) {
        *edge = edge.checked_sub(LOAD_HEADER.len())?;
    }
    Some(arm)
}

pub fn typecheck_array_arm_enabled() -> bool {
    true
}

pub fn typecheck_array_memo_enabled() -> bool {
    true
}

pub const REFERENCE_ARRAY_KIND_BYTE: u8 = (cratonvm_types::ObjectKind::Array as u8)
    | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);

#[derive(Debug)]
pub(crate) struct ArrayPrefix {
    pub(crate) bytes: Vec<u8>,
    pub(crate) hit: Vec<usize>,
    pub(crate) not_reference_array: Vec<usize>,
}

pub(crate) fn typecheck_array_prefix_bytes(component: Option<u32>) -> Option<ArrayPrefix> {
    if component == Some(0) {
        return None;
    }
    let mut b: Vec<u8> = Vec::with_capacity(32);
    let mut hit: Vec<usize> = Vec::new();
    let mut other: Vec<usize> = Vec::new();
    let rel32 = |b: &mut Vec<u8>| -> usize {
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[
        0x80,
        0x78,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
        REFERENCE_ARRAY_KIND_BYTE,
    ]);
    b.extend_from_slice(&[0x0F, 0x85]); // JNE not_reference_array
    other.push(rel32(&mut b));
    match component {
        None => {
            b.push(0xE9); // JMP hit
            hit.push(rel32(&mut b));
        }
        Some(c) => {
            b.extend_from_slice(&[0x81, 0x78, 0x00]);
            b.extend_from_slice(&c.to_le_bytes());
            b.extend_from_slice(&[0x0F, 0x84]); // JE hit
            hit.push(rel32(&mut b));
        }
    }
    Some(ArrayPrefix {
        bytes: b,
        hit,
        not_reference_array: other,
    })
}

pub fn typecheck_array_miss_enabled() -> bool {
    true
}

pub fn typecheck_array_miss_guard_enabled() -> bool {
    typecheck_array_arm_enabled() && typecheck_array_miss_enabled()
}

pub fn aastore_subtype_arm_enabled() -> bool {
    true
}

pub fn typecheck_noncall_enabled() -> bool {
    true
}

const ELEMENT_TYPE_BITS: u8 = 0x0F << 2;

#[derive(Debug)]
pub(crate) struct NotReferenceArrayMiss {
    pub(crate) bytes: Vec<u8>,
    pub(crate) slow: Vec<usize>,
    pub(crate) zero: Vec<usize>,
}

pub(crate) fn typecheck_array_not_reference_miss_bytes(latch: usize) -> NotReferenceArrayMiss {
    let array_kind = cratonvm_types::ObjectKind::Array as u8;
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
    let mut b: Vec<u8> = Vec::with_capacity(80);
    let mut slow: Vec<usize> = Vec::new();
    let mut zero: Vec<usize> = Vec::new();
    let rel32 = |b: &mut Vec<u8>| -> usize {
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
    b.extend_from_slice(&[0x0F, 0x85]);
    let to_array = rel32(&mut b);
    b.extend_from_slice(&[0x83, 0x78, 0x00, 0x00]);
    b.extend_from_slice(&[0x0F, 0x8C]);
    slow.push(rel32(&mut b));
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
    b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    b.extend_from_slice(&[0x0F, 0x85]); // JNE slow
    slow.push(rel32(&mut b));
    b.push(0xE9); // JMP zero
    zero.push(rel32(&mut b));
    let array_at = b.len();
    let rel = (array_at - (to_array + 4)) as i32;
    b[to_array..to_array + 4].copy_from_slice(&rel.to_le_bytes());
    b.extend_from_slice(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX+disp8]
    b.extend_from_slice(&[0x83, 0xE1, cratonvm_types::KIND_TAG_BYTE_MASK]); // AND ECX, imm8
    b.extend_from_slice(&[0x83, 0xF9, array_kind]); // CMP ECX, imm8
    b.extend_from_slice(&[0x0F, 0x85]); // JNE slow
    slow.push(rel32(&mut b));
    b.extend_from_slice(&[0xF6, 0x40, KIND_TAGS, ELEMENT_TYPE_BITS]); // TEST BYTE [RAX+disp8], imm8
    b.extend_from_slice(&[0x0F, 0x84]); // JZ slow
    slow.push(rel32(&mut b));
    b.push(0xE9); // JMP zero
    zero.push(rel32(&mut b));
    NotReferenceArrayMiss {
        bytes: b,
        slow,
        zero,
    }
}

pub(crate) fn typecheck_array_miss_guard_bytes(latch: usize) -> NotReferenceArrayMiss {
    let block = typecheck_array_not_reference_miss_bytes(latch);
    let mut b: Vec<u8> = Vec::with_capacity(block.bytes.len() + 10);
    b.extend_from_slice(&[
        0x80,
        0x78,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
        REFERENCE_ARRAY_KIND_BYTE,
    ]);
    b.extend_from_slice(&[0x0F, 0x84]); // JE past (rel32 = the block's length)
    // Cast: the fixed miss block above is fewer than 128 bytes.
    let past = block.bytes.len() as i32;
    b.extend_from_slice(&past.to_le_bytes());
    let at = b.len();
    b.extend_from_slice(&block.bytes);
    NotReferenceArrayMiss {
        bytes: b,
        slow: block.slow.into_iter().map(|e| at + e).collect(),
        zero: block.zero.into_iter().map(|e| at + e).collect(),
    }
}

pub(crate) fn aastore_subtype_bytes(rows: usize) -> Option<(Vec<u8>, Vec<usize>)> {
    if rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    let depth_byte = (STATUS_DEPTH_SHIFT / 8) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(64);
    let mut fall: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x41, 0x8B, 0x03]); // MOV EAX, [R11]
    b.push(0x3D); // CMP EAX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0x81, 0xFA]); // CMP EDX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0x89, 0xC1]); // MOV ECX, EAX
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0xC1, 0xE2, ROW_SHIFT]); // SHL EDX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address
    b.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x4C, 0x0B, depth_byte]); // MOVZX ECX, BYTE [R11+RCX+1]
    b.extend_from_slice(&[0x85, 0xC9]); // TEST ECX, ECX
    fall.push(jcc(&mut b, 0x84)); // JZ fall
    b.extend_from_slice(&[0x48, 0x8D, 0x14, 0x8A]); // LEA RDX, [RDX + RCX*4]
    b.extend_from_slice(&[0x41, 0x39, 0x04, 0x13]); // CMP DWORD [R11 + RDX*1], EAX
    let hit = vec![jcc(&mut b, 0x84)]; // JE hit
    let end = b.len();
    for at in fall {
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some((b, hit))
}

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_dispatch_typecheck_row_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcperf_row_first_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcarray_array_arm_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcmiss_array_miss_and_aastore_tests;

#[cfg(test)]
mod tcf21_fusion_tripwire_tests;