use super::*;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct Section {
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: usize,
    size: usize,
    link: u32,
    info: u32,
    entsize: u64,
}

fn sections(elf: &[u8]) -> Vec<Section> {
    let shoff = u64_at(elf, 0x28) as usize;
    let shentsize = u16_at(elf, 0x3A) as usize;
    let shnum = u16_at(elf, 0x3C) as usize;
    (0..shnum)
        .map(|i| {
            let h = shoff + i * shentsize;
            Section {
                name: u32_at(elf, h),
                kind: u32_at(elf, h + 4),
                flags: u64_at(elf, h + 8),
                addr: u64_at(elf, h + 16),
                offset: u64_at(elf, h + 24) as usize,
                size: u64_at(elf, h + 32) as usize,
                link: u32_at(elf, h + 40),
                info: u32_at(elf, h + 44),
                entsize: u64_at(elf, h + 56),
            }
        })
        .collect()
}

fn cstr(bytes: &[u8], at: usize) -> &str {
    let end = bytes[at..].iter().position(|&b| b == 0).expect("NUL") + at;
    std::str::from_utf8(&bytes[at..end]).expect("utf-8")
}

#[test]
fn the_object_parses_back_to_the_symbol_it_was_built_for() {
    let start = 0x7f12_3456_7000u64;
    let size = 0x2a0u64;
    let name = "java/lang/String.hashCode()I [c2]";
    let elf = build_elf(start, size, name);

    assert_eq!(&elf[0..4], b"\x7fELF");
    assert_eq!(elf[4], 2, "ELFCLASS64");
    assert_eq!(elf[5], 1, "little-endian");
    assert_eq!(u16_at(&elf, 16), 1, "ET_REL");
    assert_eq!(u16_at(&elf, 0x34) as usize, ELF_HEADER_SIZE);
    assert_eq!(u16_at(&elf, 0x3A) as usize, SECTION_HEADER_SIZE);
    assert_eq!(u16_at(&elf, 0x3C) as usize, SECTION_COUNT);
    assert_eq!(u16_at(&elf, 0x3E), SHSTRTAB_SECTION);

    let secs = sections(&elf);
    assert_eq!(secs.len(), 5);
    let shstr = &secs[SHSTRTAB_SECTION as usize];
    let shstr_bytes = &elf[shstr.offset..shstr.offset + shstr.size];
    let names: Vec<&str> = secs
        .iter()
        .map(|s| cstr(shstr_bytes, s.name as usize))
        .collect();
    assert_eq!(names, ["", ".text", ".symtab", ".strtab", ".shstrtab"]);

    let text = &secs[TEXT_SECTION as usize];
    assert_eq!(text.kind, SHT_NOBITS);
    assert_eq!(text.flags, SHF_ALLOC | SHF_EXECINSTR);
    assert_eq!(text.addr, start);
    assert_eq!(text.size as u64, size);

    let symtab = &secs[SYMTAB_SECTION as usize];
    assert_eq!(symtab.kind, SHT_SYMTAB);
    assert_eq!(symtab.entsize as usize, SYMBOL_SIZE);
    assert_eq!(symtab.size, 2 * SYMBOL_SIZE);
    assert_eq!(symtab.info, 1);
    let strtab = &secs[symtab.link as usize];
    assert_eq!(strtab.kind, SHT_STRTAB);
    let strtab_bytes = &elf[strtab.offset..strtab.offset + strtab.size];

    let sym = symtab.offset + SYMBOL_SIZE;
    assert_eq!(cstr(strtab_bytes, u32_at(&elf, sym) as usize), name);
    assert_eq!(elf[sym + 4], (STB_GLOBAL << 4) | STT_FUNC);
    assert_eq!(u16_at(&elf, sym + 6), TEXT_SECTION);
    let value = u64_at(&elf, sym + 8);
    // ET_REL: the address is the section's address plus the value.
    assert_eq!(secs[u16_at(&elf, sym + 6) as usize].addr + value, start);
    assert_eq!(u64_at(&elf, sym + 16), size);

    // Every file-backed section lies inside the image.
    for s in &secs[2..] {
        assert!(s.offset + s.size <= elf.len());
    }
    assert_ne!(text.kind, SHT_PROGBITS);
}

#[test]
fn nul_bytes_in_a_name_are_dropped() {
    let elf = build_elf(0x1000, 0x10, "a\0b");
    let secs = sections(&elf);
    let strtab = &secs[STRTAB_SECTION as usize];
    assert_eq!(&elf[strtab.offset..strtab.offset + strtab.size], b"\0ab\0");
}
