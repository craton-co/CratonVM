use super::*;

#[test]
fn the_short_form_offsets_match_the_collector_s() {
    use cratonvm_gc::tlab::JitZgcAnnounceTable as T;
    assert_eq!(ZGC_ANNOUNCE_SHORT_BLOCKED as usize, T::SHORT_BLOCKED_OFFSET);
    assert_eq!(ZGC_ANNOUNCE_GRANULE_BIAS as usize, T::GRANULE_BIAS_OFFSET);
}

/// A tiny deterministic generator (no `rand` dependency in this crate).
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `extern "C" fn(thread, obj) -> u64` around ONE form of the inline
/// store: R10 = `thread`, R11 = `obj`; returns `obj` on the inline arm and
/// 0 on every helper exit.
struct Snippet {
    c: Compiler,
    entry_off: usize,
}

impl Snippet {
    fn announce(short: bool, commit: ZgcAnnounceCommit) -> Self {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        c.emit_mov_r64_r64(R10, ARG_REGS[0]);
        c.emit_mov_r64_r64(R11, ARG_REGS[1]);
        let (done, to_helper) =
            emit_zgc_inline_announce_raw_form(&mut c.buf, 0, 8, commit, short);
        for p in to_helper {
            c.patch_rel32_to_here(p);
        }
        c.emit_xor_reg_self(RAX);
        c.buf.emit(&[0xC3]); // ret
        c.patch_rel32_to_here(done);
        c.buf.emit(&[0xC3]); // ret (RAX = the object)
        Self::finish(c, entry_off)
    }

    /// The call-free pair for two objects (R11 = `a`, R9 = `b`): returns 1
    /// on the store arm, 0 on a decline.
    fn pair(short: bool, count: bool) -> Self {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let entry_off = c.buf.pos();
        c.emit_mov_r64_r64(R10, ARG_REGS[0]);
        c.emit_mov_r64_r64(R11, ARG_REGS[1]);
        c.emit_mov_r64_r64(R9, ARG_REGS[2]);
        let mut decline: Vec<usize> = Vec::new();
        c.emit_zgc_start_bit_precheck_form(0, 8, &[R11, R9], &mut decline, count, short);
        c.emit_zgc_start_bit_store_prechecked_form(0, R11, short);
        c.emit_zgc_start_bit_store_prechecked_form(0, R9, short);
        c.emit_mov_imm32_sx(RAX, 1);
        c.buf.emit(&[0xC3]); // ret
        for p in decline {
            c.patch_rel32_to_here(p);
        }
        c.emit_xor_reg_self(RAX);
        c.buf.emit(&[0xC3]); // ret
        Self::finish(c, entry_off)
    }

    fn finish(c: Compiler, entry_off: usize) -> Self {
        assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
        assert!(c.buf.codegen_failure_reason().is_none());
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        Self { c, entry_off }
    }

    fn call3(&self, thread: u64, a: u64, b: u64) -> u64 {
        // SAFETY: `entry_off` is a byte offset inside the buffer `self.c`
        // owns, which outlives the call.
        let entry = unsafe { self.c.buf.as_ptr().add(self.entry_off) };
        // SAFETY: a leaf snippet that touches no stack, reads the thread
        // block and the table it names, writes at most one bitmap word per
        // object (and the cursor for a committing form), calls nothing,
        // and returns a u64. The caller keeps all of them alive.
        let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { std::mem::transmute(entry) };
        f(thread, a, b)
    }

    fn call(&self, thread: u64, obj: u64) -> u64 {
        self.call3(thread, obj, 0)
    }
}

/// Thread block (`cursor, end, start, owned_epoch, announce`) and table
/// (`words, base, span, blocked, epoch, owner, short_blocked,
/// granule_bias`), maintained as the collector maintains them.
struct World {
    bits: Vec<u64>,
    table: Box<[u64; 8]>,
    thread: Box<[u64; 6]>,
}

const EPOCH: u64 = 41;
const SPAN: u64 = 64 * 1024;

/// zr z11/ia: `Tlab::zgc_owned_words` as the one u64 the fake thread
/// carries (first index in the low dword), from the collector's own
/// derivation.
fn owned_words_word(base: u64, span: u64, start: u64, end: u64) -> u64 {
    let u = |v: u64| usize::try_from(v).unwrap();
    let [first, past] =
        cratonvm_gc::tlab::zgc_owned_word_window(u(base), u(span), u(start), u(end));
    u64::from(first) | (u64::from(past) << 32)
}

impl World {
    /// `base` is never dereferenced: the store reads only the thread and
    /// the table and writes only `bits`.
    fn new(base: u64, start: u64, end: u64) -> Self {
        let mut bits = vec![0u64; (SPAN / 512) as usize + 1];
        let words = bits.as_mut_ptr() as u64; // Cast: the bitmap's address
        let (bias, ok) = cratonvm_gc::tlab::JitZgcAnnounceTable::short_geometry(
            words as usize, // Cast: an address
            base as usize,  // Cast: an address
        );
        let table = Box::new([
            words,
            base,
            SPAN,
            0,
            EPOCH,
            base,
            u64::from(!ok),
            bias as u64, // Cast: an address-sized bias
        ]);
        let mut w = Self {
            bits,
            table,
            // zr z11/ia: the owned-word window last, as `Tlab::new` writes it.
            thread: Box::new([
                start,
                end,
                start,
                EPOCH,
                0,
                owned_words_word(base, SPAN, start, end),
            ]),
        };
        w.thread[4] = w.table.as_ptr() as u64; // Cast: the table's address
        w
    }

    /// `blocked`, raised or lowered as `set_jit_inline_announce_blocked`
    /// does (the short gate follows it, and stays closed on a misaligned
    /// base).
    fn set_blocked(&mut self, blocked: bool) {
        let base = self.table[1];
        self.table[3] = u64::from(blocked);
        self.table[6] = u64::from(blocked || base & 511 != 0 || self.table[0] == 0);
    }

    /// Withdrawn as `withdraw_jit_inline_announce` does.
    fn unpublish(&mut self) {
        self.table[6] = 1;
        self.table[0] = 0;
    }

    fn thread_addr(&mut self) -> u64 {
        self.thread.as_mut_ptr() as u64 // Cast: the thread block's address
    }

    /// The bit `HeapBitmap::insert` sets for `obj` (what the announce
    /// helper ends in).
    fn helper_insert(&mut self, obj: u64) {
        let bit = (obj - self.table[1]) >> 3;
        self.bits[(bit >> 6) as usize] |= 1u64 << (bit & 63); // Cast: a word index
    }
}

/// One random trial's configuration break (or none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    None,
    Unarmed,
    StaleEpoch,
    Blocked,
    Unpublished,
}

fn apply(w: &mut World, b: Break) {
    match b {
        Break::None => {}
        Break::Unarmed => w.thread[4] = 0,
        Break::StaleEpoch => w.thread[3] = EPOCH - 1,
        Break::Blocked => w.set_blocked(true),
        Break::Unpublished => w.unpublish(),
    }
}

const BASE_ALIGNED: u64 = 0x5A5A_0000_0000;

/// Random chunks, object sizes and alignments, every configuration break,
/// aligned and misaligned bitmap bases: on every allocation the short and
/// the long form either both set the bit inline or both refuse (the
/// short one also refuses on a misaligned base), and after the helper
/// serves each refusal both bitmaps hold exactly the bits the registry's
/// own insert would have set for every object -- the oracle reads the same.
#[test]
fn the_short_and_long_forms_leave_the_registry_identical_over_random_patterns() {
    let long = Snippet::announce(false, ZgcAnnounceCommit::AlreadyCommitted);
    let short = Snippet::announce(true, ZgcAnnounceCommit::AlreadyCommitted);
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let (mut inline_hits, mut helper_hits, mut objects) = (0u64, 0u64, 0u64);
    for trial in 0..600 {
        let skew = if trial % 4 == 3 {
            [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
        } else {
            0
        };
        let base = BASE_ALIGNED + skew;
        let start = base + 8 * rng.below(2048);
        let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
        let brk = match rng.below(12) {
            0 => Break::Unarmed,
            1 => Break::StaleEpoch,
            2 => Break::Blocked,
            3 => Break::Unpublished,
            _ => Break::None,
        };
        let mut wl = World::new(base, start, end);
        let mut ws = World::new(base, start, end);
        let mut expect = World::new(base, start, end);
        apply(&mut wl, brk);
        apply(&mut ws, brk);
        let mut cur = start;
        loop {
            if rng.below(5) == 0 {
                cur = (cur + 15) & !15;
            }
            let size = 16 + 8 * rng.below(32);
            if cur + size > end {
                break;
            }
            let obj = cur;
            let tl = wl.thread_addr();
            let ts = ws.thread_addr();
            let rl = long.call(tl, obj);
            let rs = short.call(ts, obj);
            assert!(rl == 0 || rl == obj, "trial {trial}: long answers obj or 0");
            assert!(
                rs == 0 || rs == obj,
                "trial {trial}: short answers obj or 0"
            );
            if skew == 0 {
                assert_eq!(
                    rl,
                    rs,
                    "trial {trial} {brk:?}: obj {:#x} in [{:#x}, {:#x})",
                    obj - base,
                    start - base,
                    end - base
                );
            } else {
                assert_eq!(
                    rs, 0,
                    "trial {trial}: a misaligned base keeps the short form shut"
                );
            }
            if rl == 0 {
                wl.helper_insert(obj);
            }
            if rs == 0 {
                ws.helper_insert(obj);
                helper_hits += 1;
            } else {
                inline_hits += 1;
            }
            expect.helper_insert(obj);
            objects += 1;
            cur += size;
        }
        assert_eq!(
            wl.bits, expect.bits,
            "trial {trial} {brk:?}: long form vs insert"
        );
        assert_eq!(
            ws.bits, expect.bits,
            "trial {trial} {brk:?}: short form vs insert"
        );
    }
    eprintln!(
        "zr9bo random: objects={objects} short_inline={inline_hits} short_helper={helper_hits}"
    );
    // Positive controls: both arms of the short form ran.
    assert!(
        inline_hits > objects / 3,
        "the short form set most bits inline"
    );
    assert!(helper_hits > 0, "and refused some");
}

/// The committing form (`ZgcAnnounceCommit::Size`): the short inline arm
/// sets the bit, then commits `obj + size`; a refusal commits nothing.
/// Same as the long form.
#[test]
fn the_short_form_commits_after_its_bit_and_never_on_a_refusal() {
    for short in [false, true] {
        let snip = Snippet::announce(short, ZgcAnnounceCommit::Size(40));
        let base = BASE_ALIGNED;
        for (brk, inline) in [
            (Break::None, true),
            (Break::Blocked, false),
            (Break::StaleEpoch, false),
        ] {
            let mut w = World::new(base, base + 1024, base + 4096);
            apply(&mut w, brk);
            let obj = base + 1024 + 64;
            w.thread[0] = obj;
            let t = w.thread_addr();
            let r = snip.call(t, obj);
            if inline {
                assert_eq!(r, obj, "short={short}: inline");
                assert_eq!(w.thread[0], obj + 40, "short={short}: committed");
                assert_eq!(
                    w.bits[((obj - base) >> 9) as usize],
                    1u64 << (((obj - base) >> 3) & 63)
                ); // Cast: word index
            } else {
                assert_eq!(r, 0, "short={short} {brk:?}: refused");
                assert_eq!(w.thread[0], obj, "short={short} {brk:?}: nothing committed");
                assert!(
                    w.bits.iter().all(|x| *x == 0),
                    "short={short} {brk:?}: no bit"
                );
            }
        }
    }
}

/// The call-free pair (`toString()`): the short pre-check + stores
/// accept and refuse exactly as the long pair on random object pairs, set
/// the same two bits, and the counted short pre-check decides as the
/// uncounted one.
#[test]
fn the_short_call_free_pair_decides_and_stores_as_the_long_pair() {
    let long = Snippet::pair(false, false);
    let short = Snippet::pair(true, false);
    let counted = Snippet::pair(true, true);
    let mut rng = XorShift(0xC0FF_EE00_1234_5678);
    let mut stored = 0u64;
    for trial in 0..2000 {
        let base = BASE_ALIGNED;
        let start = base + 8 * rng.below(1024);
        let end = (start + 8 * (4 + rng.below(1024))).min(base + SPAN);
        let a = start + 8 * rng.below((end - start) / 8);
        let b = a + 8 * (2 + rng.below(16));
        if b + 16 > end {
            continue;
        }
        let brk = match rng.below(10) {
            0 => Break::Blocked,
            1 => Break::StaleEpoch,
            2 => Break::Unpublished,
            _ => Break::None,
        };
        let mut ws = World::new(base, start, end);
        let mut wl = World::new(base, start, end);
        let mut wc = World::new(base, start, end);
        for w in [&mut ws, &mut wl, &mut wc] {
            apply(w, brk);
        }
        let (ts, tl, tc) = (ws.thread_addr(), wl.thread_addr(), wc.thread_addr());
        let rs = short.call3(ts, a, b);
        let rl = long.call3(tl, a, b);
        let rc = counted.call3(tc, a, b);
        assert_eq!(rs, rl, "trial {trial} {brk:?}");
        assert_eq!(rc, rs, "trial {trial}: counted == uncounted");
        assert_eq!(ws.bits, wl.bits, "trial {trial}: same bits");
        assert_eq!(wc.bits, ws.bits, "trial {trial}: same bits (counted)");
        if rs == 1 {
            let mut e = World::new(base, start, end);
            e.helper_insert(a);
            e.helper_insert(b);
            assert_eq!(ws.bits, e.bits, "trial {trial}: exactly a's and b's bits");
            stored += 1;
        } else {
            assert!(
                ws.bits.iter().all(|x| *x == 0),
                "trial {trial}: a decline writes nothing"
            );
        }
    }
    assert!(stored > 100, "the store arm ran ({stored})");
    // A misaligned base: the long pair may store, the short one declines.
    let base = BASE_ALIGNED + 8;
    let mut w = World::new(base, base + 1024, base + 8192);
    let t = w.thread_addr();
    assert_eq!(short.call3(t, base + 2048, base + 2096), 0);
    assert!(w.bits.iter().all(|x| *x == 0));
}




// ---- zr z11/ia: the trimmed forms ------------------------------------

/// Which store a zr z11/ia snippet runs.
#[derive(Clone, Copy, Debug)]
enum Form {
    Long(LongAnnounceTrims),
    Short { rax_end: bool },
}

/// `extern "C" fn(thread, obj, end) -> u64` around one explicitly trimmed
/// form: R10 = `thread`, R11 = `obj`, RAX = `end` (the bump's new cursor,
/// what the optimizing tier leaves there); returns `obj` on the inline arm
/// and 0 on every helper exit. No environment read.
fn trimmed(form: Form, commit: ZgcAnnounceCommit) -> Snippet {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    c.emit_mov_r64_r64(R10, ARG_REGS[0]);
    c.emit_mov_r64_r64(R11, ARG_REGS[1]);
    c.emit_mov_r64_r64(RAX, ARG_REGS[2]);
    let (done, to_helper) = match form {
        Form::Long(t) => emit_zgc_inline_announce_long_raw_trimmed(&mut c.buf, 0, 8, commit, t),
        Form::Short { rax_end } => {
            emit_zgc_inline_announce_short_raw_trimmed(&mut c.buf, 0, 8, commit, rax_end)
        }
    };
    for p in to_helper {
        c.patch_rel32_to_here(p);
    }
    c.emit_xor_reg_self(RAX);
    c.buf.emit(&[0xC3]); // ret
    c.patch_rel32_to_here(done);
    c.buf.emit(&[0xC3]); // ret (RAX = the object)
    Snippet::finish(c, entry_off)
}

fn snippet_len(form: Form, commit: ZgcAnnounceCommit) -> usize {
    let mut buf = crate::ExecutableBuffer::new(1024).expect("buffer");
    match form {
        Form::Long(t) => emit_zgc_inline_announce_long_raw_trimmed(&mut buf, 0, 8, commit, t),
        Form::Short { rax_end } => {
            emit_zgc_inline_announce_short_raw_trimmed(&mut buf, 0, 8, commit, rax_end)
        }
    };
    buf.pos()
}

const ALL_LONG_TRIMS: LongAnnounceTrims = LongAnnounceTrims {
    owned_window: true,
    skip_grid: true,
    rax_end: true,
    skip_span: true,
};

/// zr z11/ia: every trimmed form against its untrimmed twin over random
/// chunks, sizes, configuration breaks and bitmap bases (aligned and
/// skewed by multiples of 8): on every allocation both answer the same
/// (`obj` or 0), commit the same cursor (`obj + size` on the inline arm,
/// nothing on a refusal), and after the helper serves each refusal hold
/// exactly the bits the registry's own insert sets.
#[test]
fn zr11ia_trimmed_forms_decide_and_commit_as_the_untrimmed_ones() {
    const SIZE: u64 = 40;
    let commit = ZgcAnnounceCommit::Size(40);
    let pairs: [(&str, Form, Form, ZgcAnnounceCommit); 6] = [
        (
            "long window+span skip only, size (zr s1/ca)",
            Form::Long(LongAnnounceTrims::NONE),
            Form::Long(LongAnnounceTrims {
                owned_window: true,
                skip_grid: false,
                rax_end: false,
                skip_span: true,
            }),
            commit,
        ),
        (
            "long window+grid, committed",
            Form::Long(LongAnnounceTrims::NONE),
            Form::Long(LongAnnounceTrims {
                owned_window: true,
                skip_grid: true,
                rax_end: false,
                skip_span: false,
            }),
            ZgcAnnounceCommit::AlreadyCommitted,
        ),
        (
            "long window only, size",
            Form::Long(LongAnnounceTrims::NONE),
            Form::Long(LongAnnounceTrims {
                owned_window: true,
                skip_grid: false,
                rax_end: false,
                skip_span: false,
            }),
            commit,
        ),
        (
            "long all trims, size",
            Form::Long(LongAnnounceTrims::NONE),
            Form::Long(ALL_LONG_TRIMS),
            commit,
        ),
        (
            "long rax_end without the window (inert)",
            Form::Long(LongAnnounceTrims::NONE),
            Form::Long(LongAnnounceTrims {
                owned_window: false,
                skip_grid: true,
                rax_end: true,
                skip_span: false,
            }),
            commit,
        ),
        (
            "short rax_end, size",
            Form::Short { rax_end: false },
            Form::Short { rax_end: true },
            commit,
        ),
    ];
    for (name, reference, candidate, commit) in pairs {
        let committing = commit != ZgcAnnounceCommit::AlreadyCommitted;
        let a = trimmed(reference, commit);
        let b = trimmed(candidate, commit);
        let mut rng = XorShift(0x2111_1A00_0000_0001);
        let (mut inline, mut refused) = (0u64, 0u64);
        for trial in 0..400 {
            let skew = if trial % 4 == 3 {
                [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
            } else {
                0
            };
            let base = BASE_ALIGNED + skew;
            let start = base + 8 * rng.below(2048);
            let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
            let brk = match rng.below(12) {
                0 => Break::Unarmed,
                1 => Break::StaleEpoch,
                2 => Break::Blocked,
                3 => Break::Unpublished,
                _ => Break::None,
            };
            let mut wa = World::new(base, start, end);
            let mut wb = World::new(base, start, end);
            let mut expect = World::new(base, start, end);
            apply(&mut wa, brk);
            apply(&mut wb, brk);
            let mut cur = start;
            loop {
                if rng.below(5) == 0 {
                    cur = (cur + 15) & !15;
                }
                let size = if committing {
                    SIZE
                } else {
                    16 + 8 * rng.below(32)
                };
                if cur + size > end {
                    break;
                }
                let obj = cur;
                wa.thread[0] = obj;
                wb.thread[0] = obj;
                let (ta, tb) = (wa.thread_addr(), wb.thread_addr());
                let ra = a.call3(ta, obj, obj + size);
                let rb = b.call3(tb, obj, obj + size);
                assert!(ra == 0 || ra == obj, "{name} trial {trial}: obj or 0");
                assert_eq!(
                    ra,
                    rb,
                    "{name} trial {trial} {brk:?}: obj {:#x} in [{:#x}, {:#x})",
                    obj - base,
                    start - base,
                    end - base
                );
                let committed = if committing && ra != 0 {
                    obj + size
                } else {
                    obj
                };
                assert_eq!(
                    wa.thread[0], committed,
                    "{name} trial {trial}: reference cursor"
                );
                assert_eq!(wb.thread[0], committed, "{name} trial {trial}: cursor");
                if ra == 0 {
                    wa.helper_insert(obj);
                    wb.helper_insert(obj);
                    refused += 1;
                } else {
                    inline += 1;
                }
                expect.helper_insert(obj);
                cur += size;
            }
            assert_eq!(
                wa.bits, expect.bits,
                "{name} trial {trial}: reference vs insert"
            );
            assert_eq!(
                wb.bits, expect.bits,
                "{name} trial {trial}: trimmed vs insert"
            );
        }
        eprintln!("zr11ia {name}: inline={inline} refused={refused}");
        // Positive controls: both arms ran.
        assert!(inline > 1000, "{name}: the inline arm ran ({inline})");
        assert!(refused > 100, "{name}: the helper arm ran ({refused})");
    }
}

/// zr z11/ia positive control: the trims emit fewer bytes, and with every
/// trim off the long form is the round-9 sequence byte for byte
/// (`emit_zgc_inline_announce_long_raw`, which the r16/nio21 byte tests
/// pin against the single-pass copy).
#[test]
fn zr11ia_trims_shorten_the_sequence_and_none_is_the_old_bytes() {
    let size = ZgcAnnounceCommit::Size(40);
    let array = ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 3 };
    let none = Form::Long(LongAnnounceTrims::NONE);
    let window_grid = Form::Long(LongAnnounceTrims {
        owned_window: true,
        skip_grid: true,
        rax_end: false,
        skip_span: false,
    });
    for commit in [size, array, ZgcAnnounceCommit::AlreadyCommitted] {
        let n = snippet_len(none, commit);
        let w = snippet_len(window_grid, commit);
        // Ten instructions of granule arithmetic -> two compares, the AND
        // and the grid test gone.
        assert!(n >= w + 20, "{commit:?}: {n} -> {w}");
    }
    let all = Form::Long(ALL_LONG_TRIMS);
    assert!(snippet_len(all, array) + 10 < snippet_len(window_grid, array));
    assert!(snippet_len(all, size) < snippet_len(window_grid, size));
    assert!(
        snippet_len(Form::Short { rax_end: true }, array)
            < snippet_len(Form::Short { rax_end: false }, array)
    );
    // `AlreadyCommitted` has no commit to move: rax_end changes nothing.
    let ac = ZgcAnnounceCommit::AlreadyCommitted;
    let all_but_rax = Form::Long(LongAnnounceTrims {
        rax_end: false,
        ..ALL_LONG_TRIMS
    });
    assert_eq!(snippet_len(all, ac), snippet_len(all_but_rax, ac));
    // zr s1/ca: the span skip takes the `jb`, the `cmp [span]` and the
    // `jae` (6 + 7 + 6 bytes) on every commit kind.
    for commit in [size, array, ac] {
        assert_eq!(
            snippet_len(window_grid, commit),
            snippet_len(
                Form::Long(LongAnnounceTrims {
                    skip_span: true,
                    ..match window_grid {
                        Form::Long(t) => t,
                        Form::Short { .. } => unreachable!(),
                    }
                }),
                commit
            ) + 19,
            "{commit:?}"
        );
    }
    let mut a = crate::ExecutableBuffer::new(1024).expect("buffer");
    let mut b = crate::ExecutableBuffer::new(1024).expect("buffer");
    let ra = emit_zgc_inline_announce_long_raw(&mut a, 0x40, 0x48, size);
    let rb = emit_zgc_inline_announce_long_raw_trimmed(
        &mut b,
        0x40,
        0x48,
        size,
        LongAnnounceTrims::NONE,
    );
    assert_eq!(a.as_slice(), b.as_slice());
    assert_eq!(ra, rb);
}

// ---- rx27 wave 3b (lane alloc): the lean start-bit forms -------------

const LEAN_GATE: LeanStartBit = LeanStartBit {
    short_gate: true,
    bts: false,
};
const LEAN_BTS: LeanStartBit = LeanStartBit {
    short_gate: false,
    bts: true,
};
const LEAN_BOTH: LeanStartBit = LeanStartBit {
    short_gate: true,
    bts: true,
};

/// [`trimmed`] for the long store with every z11/s1 trim and `lean`.
fn lean_long(lean: LeanStartBit, commit: ZgcAnnounceCommit) -> Snippet {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    c.emit_mov_r64_r64(R10, ARG_REGS[0]);
    c.emit_mov_r64_r64(R11, ARG_REGS[1]);
    c.emit_mov_r64_r64(RAX, ARG_REGS[2]);
    let (done, to_helper) =
        emit_zgc_inline_announce_long_raw_lean(&mut c.buf, 0, 8, commit, ALL_LONG_TRIMS, lean);
    for p in to_helper {
        c.patch_rel32_to_here(p);
    }
    c.emit_xor_reg_self(RAX);
    c.buf.emit(&[0xC3]); // ret
    c.patch_rel32_to_here(done);
    c.buf.emit(&[0xC3]); // ret (RAX = the object)
    Snippet::finish(c, entry_off)
}

/// [`Snippet::pair`] through the shared (optimizing-tier) lean pre-check
/// and stores: 1 on the store arm, 0 on a decline.
fn lean_pair(bts: bool) -> Snippet {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    c.emit_mov_r64_r64(R10, ARG_REGS[0]);
    c.emit_mov_r64_r64(R11, ARG_REGS[1]);
    c.emit_mov_r64_r64(R9, ARG_REGS[2]);
    let mut decline: Vec<usize> = Vec::new();
    super::super::concat_emit::emit_zgc_start_bit_precheck_lean(
        &mut c.buf,
        0,
        &[R11, R9],
        &mut decline,
    );
    super::super::concat_emit::emit_zgc_start_bit_store_prechecked_lean(
        &mut c.buf, 0, R11, bts,
    );
    super::super::concat_emit::emit_zgc_start_bit_store_prechecked_lean(&mut c.buf, 0, R9, bts);
    c.emit_mov_imm32_sx(RAX, 1);
    c.buf.emit(&[0xC3]); // ret
    for p in decline {
        c.patch_rel32_to_here(p);
    }
    c.emit_xor_reg_self(RAX);
    c.buf.emit(&[0xC3]); // ret
    Snippet::finish(c, entry_off)
}

fn long_lean_bytes(lean: LeanStartBit) -> Vec<u8> {
    let mut buf = crate::ExecutableBuffer::new(1024).expect("buffer");
    emit_zgc_inline_announce_long_raw_lean(
        &mut buf,
        0x40,
        0x48,
        ZgcAnnounceCommit::Size(40),
        ALL_LONG_TRIMS,
        lean,
    );
    buf.as_slice().to_vec()
}

/// `hay` with its ONE occurrence of `from` replaced by `to`.
fn splice_once(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let at: Vec<usize> = hay
        .windows(from.len())
        .enumerate()
        .filter(|(_, w)| *w == from)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "exactly one occurrence of {from:02X?}");
    let mut out = hay[..at[0]].to_vec();
    out.extend_from_slice(to);
    out.extend_from_slice(&hay[at[0] + from.len()..]);
    out
}

/// The long store's shape under each lean choice, against the default
/// (every z11/s1 trim, RAX = the end, table in R9): exactly one group
/// replaced, nothing else moved.
/// * gate: `MOV RDX, [R9 + blocked]; TEST RDX, RDX; JNE` (3 instructions)
///   -> `CMP QWORD [R9 + short_blocked], 0; JNE` (2);
/// * bts: `MOV R9D, 1; SHL R9, CL; OR [RDX + R8*8], R9` (3) ->
///   `BTS QWORD [RDX], RCX` (1).
/// The default store is 29 instructions with its commit and exit; both
/// choices make it 26.
#[test]
fn rx27_lean_long_store_replaces_exactly_the_gate_and_the_or() {
    let none = long_lean_bytes(LeanStartBit::NONE);
    let mut explicit = crate::ExecutableBuffer::new(1024).expect("buffer");
    emit_zgc_inline_announce_long_raw_trimmed(
        &mut explicit,
        0x40,
        0x48,
        ZgcAnnounceCommit::Size(40),
        ALL_LONG_TRIMS,
    );
    assert_eq!(none, explicit.as_slice(), "NONE is the default bytes");
    let blocked: [u8; 16] = [
        0x49, 0x8B, 0x91, 0x18, 0, 0, 0, 0x48, 0x85, 0xD2, 0x0F, 0x85, 0, 0, 0, 0,
    ];
    let gate: [u8; 14] = [0x49, 0x83, 0xB9, 0x30, 0, 0, 0, 0, 0x0F, 0x85, 0, 0, 0, 0];
    let or: [u8; 13] = [
        0x41, 0xB9, 1, 0, 0, 0, 0x49, 0xD3, 0xE1, 0x4E, 0x09, 0x0C, 0xC2,
    ];
    assert_eq!(
        long_lean_bytes(LEAN_GATE),
        splice_once(&none, &blocked, &gate)
    );
    assert_eq!(
        long_lean_bytes(LEAN_BTS),
        splice_once(&none, &or, &BTS_QWORD_RDX_RCX)
    );
    let both = splice_once(
        &splice_once(&none, &blocked, &gate),
        &or,
        &BTS_QWORD_RDX_RCX,
    );
    assert_eq!(long_lean_bytes(LEAN_BOTH), both);
}

/// Every lean long store against the default one over random chunks,
/// configuration breaks and aligned / skewed bitmap bases: the same answer
/// (`obj` or 0) and the same committed cursor, except that the
/// `short_blocked` gate also declines on a base that is not 512-aligned
/// (the collector keeps it closed there); after the helper serves every
/// refusal, both bitmaps hold exactly the registry's own insert bits.
#[test]
fn rx27_lean_long_store_decides_and_commits_as_the_default_one() {
    const SIZE: u64 = 40;
    let commit = ZgcAnnounceCommit::Size(40);
    let reference = lean_long(LeanStartBit::NONE, commit);
    for lean in [LEAN_GATE, LEAN_BTS, LEAN_BOTH] {
        let cand = lean_long(lean, commit);
        let mut rng = XorShift(0x2727_A110_C000_0001);
        let (mut inline, mut refused, mut skew_declines) = (0u64, 0u64, 0u64);
        for trial in 0..400 {
            let skew = if trial % 4 == 3 {
                [8u64, 64, 264, 504][rng.below(4) as usize] // Cast: a small index
            } else {
                0
            };
            let base = BASE_ALIGNED + skew;
            let start = base + 8 * rng.below(2048);
            let end = (start + 8 * (8 + rng.below(4096))).min(base + SPAN);
            let brk = match rng.below(12) {
                0 => Break::Unarmed,
                1 => Break::StaleEpoch,
                2 => Break::Blocked,
                3 => Break::Unpublished,
                _ => Break::None,
            };
            let mut wa = World::new(base, start, end);
            let mut wb = World::new(base, start, end);
            let mut expect = World::new(base, start, end);
            apply(&mut wa, brk);
            apply(&mut wb, brk);
            let mut cur = start;
            loop {
                if rng.below(5) == 0 {
                    cur = (cur + 15) & !15;
                }
                if cur + SIZE > end {
                    break;
                }
                let obj = cur;
                wa.thread[0] = obj;
                wb.thread[0] = obj;
                let (ta, tb) = (wa.thread_addr(), wb.thread_addr());
                let ra = reference.call3(ta, obj, obj + SIZE);
                let rb = cand.call3(tb, obj, obj + SIZE);
                assert!(rb == 0 || rb == obj, "{lean:?} trial {trial}: obj or 0");
                if lean.short_gate && skew != 0 {
                    assert_eq!(rb, 0, "{lean:?} trial {trial}: skewed base declines");
                    skew_declines += u64::from(ra != 0);
                } else {
                    assert_eq!(ra, rb, "{lean:?} trial {trial} {brk:?}");
                }
                let committed = |r: u64| if r != 0 { obj + SIZE } else { obj };
                assert_eq!(
                    wa.thread[0],
                    committed(ra),
                    "trial {trial}: reference cursor"
                );
                assert_eq!(
                    wb.thread[0],
                    committed(rb),
                    "{lean:?} trial {trial}: cursor"
                );
                if ra == 0 {
                    wa.helper_insert(obj);
                }
                if rb == 0 {
                    wb.helper_insert(obj);
                    refused += 1;
                } else {
                    inline += 1;
                }
                expect.helper_insert(obj);
                cur += SIZE;
            }
            assert_eq!(wa.bits, expect.bits, "trial {trial}: reference vs insert");
            assert_eq!(
                wb.bits, expect.bits,
                "{lean:?} trial {trial}: lean vs insert"
            );
        }
        eprintln!(
            "rx27 lean long {lean:?}: inline={inline} refused={refused} skew={skew_declines}"
        );
        assert!(inline > 1000, "{lean:?}: the inline arm ran ({inline})");
        assert!(refused > 100, "{lean:?}: the helper arm ran ({refused})");
    }
}

/// The optimizing tier's lean call-free pair (the substring `byte[]` +
/// `String` bump, `toString()`, the `char[]` copies) accepts and refuses
/// exactly as the default long pair on random object pairs over an
/// aligned base, sets exactly the two bits, writes nothing on a decline,
/// and declines on a skewed base (the `short_blocked` gate).
#[test]
fn rx27_lean_pair_decides_and_stores_as_the_long_pair() {
    let long = Snippet::pair(false, false);
    for bts in [false, true] {
        let lean = lean_pair(bts);
        let mut rng = XorShift(0x2727_0FA1_2345_6789);
        let mut stored = 0u64;
        for trial in 0..2000 {
            let base = BASE_ALIGNED;
            let start = base + 8 * rng.below(1024);
            let end = (start + 8 * (4 + rng.below(1024))).min(base + SPAN);
            let a = start + 8 * rng.below((end - start) / 8);
            let b = a + 8 * (2 + rng.below(16));
            if b + 16 > end {
                continue;
            }
            let brk = match rng.below(10) {
                0 => Break::Blocked,
                1 => Break::StaleEpoch,
                2 => Break::Unpublished,
                3 => Break::Unarmed,
                _ => Break::None,
            };
            let mut wl = World::new(base, start, end);
            let mut wn = World::new(base, start, end);
            apply(&mut wl, brk);
            apply(&mut wn, brk);
            let (tl, tn) = (wl.thread_addr(), wn.thread_addr());
            let rl = long.call3(tl, a, b);
            let rn = lean.call3(tn, a, b);
            assert_eq!(rn, rl, "bts={bts} trial {trial} {brk:?}");
            assert_eq!(wn.bits, wl.bits, "bts={bts} trial {trial}: same bits");
            if rn == 1 {
                let mut e = World::new(base, start, end);
                e.helper_insert(a);
                e.helper_insert(b);
                assert_eq!(wn.bits, e.bits, "trial {trial}: exactly a's and b's bits");
                stored += 1;
            } else {
                assert!(wn.bits.iter().all(|x| *x == 0), "trial {trial}: decline");
            }
        }
        assert!(stored > 100, "bts={bts}: the store arm ran ({stored})");
        let base = BASE_ALIGNED + 8;
        let mut w = World::new(base, base + 1024, base + 8192);
        let t = w.thread_addr();
        assert_eq!(lean.call3(t, base + 2048, base + 2096), 0);
        assert!(w.bits.iter().all(|x| *x == 0));
    }
}

/// The pair's size: per object the lean pre-check drops the span, grid and
/// granule arithmetic (13 -> 7 instructions) and the store its
/// `AND RCX, 63` (11 -> 10; with `BTS` 6). Fewer bytes on every count of
/// objects, and the shared part is one instruction shorter.
#[test]
fn rx27_lean_pair_is_shorter_than_the_long_pair() {
    let len = |lean: bool, bts: bool, objs: &[u8]| {
        let mut buf = crate::ExecutableBuffer::new(2048).expect("buffer");
        let mut d = Vec::new();
        if lean {
            super::super::concat_emit::emit_zgc_start_bit_precheck_lean(
                &mut buf, 0x40, objs, &mut d,
            );
        } else {
            super::super::concat_emit::emit_zgc_start_bit_precheck_form(
                &mut buf, 0x40, 0x48, objs, &mut d, false,
            );
        }
        let decl = d.len();
        for &o in objs {
            if lean {
                super::super::concat_emit::emit_zgc_start_bit_store_prechecked_lean(
                    &mut buf, 0x40, o, bts,
                );
            } else {
                super::super::concat_emit::emit_zgc_start_bit_store_prechecked_form(
                    &mut buf, 0x40, o, false,
                );
            }
        }
        (buf.pos(), decl)
    };
    for objs in [&[R11][..], &[R11, R9][..]] {
        let (old, old_d) = len(false, false, objs);
        let (lean, lean_d) = len(true, false, objs);
        let (bts, _) = len(true, true, objs);
        assert!(lean < old, "{objs:?}: {old} -> {lean}");
        assert!(bts < lean, "{objs:?}: {lean} -> {bts}");
        // Branches: shared 4 in both; per object 5 (long) -> 2 (window).
        assert_eq!(old_d, 4 + 5 * objs.len());
        assert_eq!(lean_d, 4 + 2 * objs.len());        }    }