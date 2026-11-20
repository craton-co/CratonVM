use super::*;
use std::alloc::System;

/// A zeroed, page-aligned region from the system allocator to run an
/// arena over.
struct Region {
    ptr: *mut u8,
    layout: Layout,
}

impl Region {
    fn new(len: usize) -> Region {
        let layout = Layout::from_size_align(len, PAGE).unwrap();
        // SAFETY: non-zero size.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Region { ptr, layout }
    }

    fn heap(&self) -> ArenaHeap {
        // SAFETY: a fresh zeroed region we own.
        unsafe { ArenaHeap::init(self.ptr, self.layout.size()) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { System.dealloc(self.ptr, self.layout) };
    }
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}

fn lay(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// Fill a block with a pattern derived from `seed`.
unsafe fn fill(p: *mut u8, n: usize, seed: u64) {
    for i in 0..n {
        unsafe { *p.add(i) = (seed as usize).wrapping_mul(31).wrapping_add(i) as u8 };
    }
}

unsafe fn check(p: *const u8, n: usize, seed: u64) -> bool {
    (0..n)
        .all(|i| unsafe { *p.add(i) } == (seed as usize).wrapping_mul(31).wrapping_add(i) as u8)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn size_classes_cover_every_small_size() {
    assert_eq!(class_size(NUM_SMALL - 1), MAX_SMALL);
    let mut prev = 0;
    for c in 0..NUM_SMALL {
        let s = class_size(c);
        assert!(s > prev && s % 16 == 0, "class {c} = {s}");
        assert_eq!(class_of(s), c, "class_of(class_size({c}))");
        prev = s;
    }
    for s in 1..=MAX_SMALL {
        let c = class_of(s);
        assert!(
            class_size(c) >= s && (c == 0 || class_size(c - 1) < s),
            "size {s} -> class {c}"
        );
    }
}

#[test]
fn free_ranges_name_exactly_the_freed_blocks_and_spans() {
    let r = Region::new(16 << 20);
    let h = r.heap();
    unsafe {
        let small = h.alloc(lay(100, 8));
        let keep = h.alloc(lay(100, 8));
        let big = h.alloc(lay(200_000, 8));
        let tail = h.alloc(lay(40_000, 8)); // keeps `big` off the bump pointer
        assert!(h.free_ranges().is_empty());
        h.dealloc(small, lay(100, 8));
        h.dealloc(big, lay(200_000, 8));
        let f = h.free_ranges();
        let at = |p: *mut u8| f.iter().any(|x| x.contains(&(p as usize)));
        assert!(at(small) && at(big), "{f:?}");
        assert!(!at(keep) && !at(tail), "{f:?}");
        assert!(f.windows(2).all(|w| w[0].end <= w[1].start), "sorted, disjoint: {f:?}");
    }
}

#[test]
fn pointer_free_pages_are_exactly_the_low_alignment_blocks() {
    let r = Region::new(16 << 20);
    let h = r.heap();
    unsafe {
        let bytes = h.alloc(lay(100, 1));
        let word = h.alloc(lay(100, 8));
        let big_bytes = h.alloc(lay(200_000, 2));
        let big_word = h.alloc(lay(200_000, 8));
        let f = h.pointer_free_ranges();
        let at = |p: *mut u8| f.iter().any(|x| x.contains(&(p as usize)));
        assert!(at(bytes) && at(big_bytes) && at(big_bytes.add(199_999)), "{f:?}");
        assert!(!at(word) && !at(big_word), "{f:?}");
        // A freed byte span stops being pointer-free once its pages come back as words.
        h.dealloc(big_bytes, lay(200_000, 2));
        let reused = h.alloc(lay(200_000, 8));
        assert!(!h.pointer_free_ranges().iter().any(|x| x.contains(&(reused as usize))));
        // A byte span grown in place at the bump pointer stays pointer-free.
        let top = h.alloc(lay(100_000, 1));
        let grown = h.realloc(top, lay(100_000, 1), 300_000);
        let f = h.pointer_free_ranges();
        assert!(f.iter().any(|x| x.contains(&(grown as usize + 299_999))), "{f:?}");
    }
}

#[test]
fn a_reused_block_comes_back_cleared_whole() {
    let r = Region::new(16 << 20);
    let h = r.heap();
    unsafe {
        for (size, align) in [(24usize, 8usize), (24, 1), (3000, 8), (100_000, 8)] {
            let l = lay(size, align);
            let p = h.alloc(l);
            let whole = Bucket::of(size, align).bytes();
            std::ptr::write_bytes(p, 0xAA, whole);
            h.dealloc(p, l);
            let q = h.alloc(l);
            assert_eq!(q, p);
            assert!((0..whole).all(|i| *q.add(i) == 0), "size {size} align {align}");
        }
    }
}

#[test]
fn alloc_free_reuses_blocks() {
    let r = Region::new(16 << 20);
    let h = r.heap();
    unsafe {
        let mut v = Vec::new();
        for (i, s) in [1usize, 8, 16, 17, 100, 129, 1000, 4096, 20000, 32768]
            .iter()
            .enumerate()
        {
            let p = h.alloc(lay(*s, 8));
            assert!(h.owns(p) && p as usize >= r.ptr as usize + header_len(r.layout.size()));
            fill(p, *s, i as u64);
            v.push((p, *s, i as u64));
        }
        for &(p, s, seed) in &v {
            assert!(check(p, s, seed), "block of {s} clobbered");
        }
        for &(p, s, _) in &v {
            h.dealloc(p, lay(s, 8));
        }
        // LIFO free lists: the same size comes back at the same address.
        for &(p, s, _) in v.iter().rev() {
            assert_eq!(h.alloc(lay(s, 8)), p, "size {s} not reused");
        }
        let st = h.stats();
        assert_eq!(st.allocs, 20);
        assert_eq!(st.frees, 10);
    }
}

#[test]
fn alignment_is_honoured_small_and_large() {
    let r = Region::new(64 << 20);
    let h = r.heap();
    unsafe {
        for shift in 0..=21 {
            let align = 1usize << shift;
            for size in [1usize, 3, 16, 48, 100, 4000, 5000, 40000] {
                let l = lay(size, align);
                let p = h.alloc(l);
                assert!(!p.is_null(), "size {size} align {align}");
                assert_eq!(p as usize % align, 0, "size {size} align {align} -> {p:p}");
                fill(p, size, 7);
                assert!(check(p, size, 7));
                if size % 2 == 0 {
                    h.dealloc(p, l);
                    // Freed then re-requested: still aligned.
                    let q = h.alloc(l);
                    assert_eq!(q as usize % align, 0);
                }
            }
        }
    }
}

#[test]
fn realloc_preserves_contents_across_buckets() {
    let r = Region::new(64 << 20);
    let h = r.heap();
    unsafe {
        let mut l = lay(10, 8);
        let mut p = h.alloc(l);
        fill(p, 10, 3);
        // Same class: in place.
        assert_eq!(h.realloc(p, l, 16), p);
        l = lay(16, 8);
        let (mut size, mut seed) = (10, 3);
        for next in [100usize, 4000, 33000, 200_000, 1 << 20, 3 << 20] {
            let q = h.realloc(p, l, next);
            assert!(!q.is_null());
            assert!(check(q, size, seed), "{size} -> {next}");
            seed = 4;
            fill(q, next, seed);
            p = q;
            l = lay(next, 8);
            size = next;
        }
        // The last large block is at the bump top: grows in place.
        let q = h.realloc(p, l, 5 << 20);
        assert_eq!(q, p, "top-of-bump grow should be in place");
        assert!(check(q, 3 << 20, 4));
        l = lay(5 << 20, 8);
        // Shrink in place, then the freed tail is reusable.
        let q = h.realloc(p, l, 1 << 20);
        assert_eq!(q, p);
        l = lay(1 << 20, 8);
        assert!(check(q, 1 << 20, 4));
        let tail = h.alloc(lay(2 << 20, 8));
        assert!(
            tail as usize >= p as usize + (1 << 20) && (tail as usize) < p as usize + (5 << 20)
        );
        // Large -> small.
        let q = h.realloc(p, l, 50);
        assert!(check(q, 50, 4));
        assert!(h.stats().in_place_reallocs >= 3);
    }
}

#[test]
fn large_blocks_reuse_spans_and_split() {
    let r = Region::new(64 << 20);
    let h = r.heap();
    unsafe {
        let a = h.alloc(lay(4 << 20, 8));
        let _guard = h.alloc(lay(64, 8));
        h.dealloc(a, lay(4 << 20, 8));
        // Fits in the freed 4 MiB span: first fit, at its start.
        let b = h.alloc(lay(1 << 20, 8));
        assert_eq!(b, a);
        // An aligned request carves from the same span past an unaligned prefix.
        let c = h.alloc(lay(1 << 20, 1 << 20));
        assert_eq!(c as usize % (1 << 20), 0);
        assert!(c as usize >= a as usize && (c as usize) < a as usize + (4 << 20));
        fill(b, 1 << 20, 1);
        fill(c, 1 << 20, 2);
        assert!(check(b, 1 << 20, 1) && check(c, 1 << 20, 2));
    }
}

#[test]
fn alloc_zeroed_clears_reused_blocks_only() {
    let r = Region::new(16 << 20);
    let h = r.heap();
    unsafe {
        for size in [24usize, 3000, 100_000] {
            let l = lay(size, 8);
            let p = h.alloc(l);
            std::ptr::write_bytes(p, 0xAA, size);
            h.dealloc(p, l);
            let q = h.alloc_zeroed(l);
            assert_eq!(q, p);
            assert!((0..size).all(|i| *q.add(i) == 0), "size {size} not zeroed");
            let f = h.alloc_zeroed(l);
            assert!(
                (0..size).all(|i| *f.add(i) == 0),
                "fresh size {size} not zero"
            );
        }
    }
}

#[test]
fn full_arena_returns_null() {
    let r = Region::new(64 * 1024);
    let h = r.heap();
    unsafe {
        let mut n = 0;
        while !h.alloc(lay(1000, 8)).is_null() {
            n += 1;
            assert!(n < 100);
        }
        assert!(n >= 50, "only {n} blocks of 1000 in 60 KiB");
        assert!(h.alloc(lay(1 << 20, 8)).is_null());
        assert!(h.used_len() <= 64 * 1024);
    }
}

#[test]
fn concurrent_threads_never_share_a_block() {
    let r = Region::new(512 << 20);
    let h = r.heap();
    std::thread::scope(|s| {
        for t in 0..8u64 {
            s.spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t + 1));
                let mut live: Vec<(usize, usize, usize, u64)> = Vec::new();
                for i in 0..20_000u64 {
                    let x = rng.next();
                    if live.len() < 64 && (x % 3 != 0 || live.is_empty()) {
                        let size = match x % 10 {
                            0 => 33_000 + (x >> 8) as usize % 200_000,
                            1..=3 => 1 + (x >> 8) as usize % 4000,
                            _ => 1 + (x >> 8) as usize % 200,
                        };
                        let align = 1usize << ((x >> 40) % 8);
                        let seed = t << 32 | i;
                        unsafe {
                            let p = h.alloc(lay(size, align));
                            assert!(!p.is_null());
                            assert_eq!(p as usize % align, 0);
                            fill(p, size, seed);
                            live.push((p as usize, size, align, seed));
                        }
                    } else {
                        let k = (x >> 20) as usize % live.len();
                        let (p, size, align, seed) = live.swap_remove(k);
                        unsafe {
                            assert!(
                                check(p as *const u8, size, seed),
                                "thread {t}: block clobbered"
                            );
                            if x % 7 == 0 {
                                let ns = size * 2 + 1;
                                let q = h.realloc(p as *mut u8, lay(size, align), ns);
                                assert!(check(q, size, seed), "thread {t}: realloc lost data");
                                h.dealloc(q, lay(ns, align));
                            } else {
                                h.dealloc(p as *mut u8, lay(size, align));
                            }
                        }
                    }
                }
                for (p, size, align, seed) in live {
                    unsafe {
                        assert!(check(p as *const u8, size, seed));
                        h.dealloc(p as *mut u8, lay(size, align));
                    }
                }
            });
        }
    });
    let st = h.stats();
    assert!(st.allocs > 50_000, "{st:?}");
    assert_eq!(st.bytes_live, 0, "{st:?}");
    assert_eq!(st.allocs, st.frees, "{st:?}");
    // Coalescing and bump retraction: with everything freed, the large
    // spans have folded back; the used part stays a small multiple of
    // the peak (small blocks stay on their class lists).
    assert!(
        (st.used_len as u64) < 4 * st.bytes_peak,
        "fragmentation: {st:?}"
    );
}

#[test]
fn attach_sees_the_same_state_and_refuses_garbage() {
    let r = Region::new(1 << 20);
    let h = r.heap();
    unsafe {
        let p = h.alloc(lay(100, 8));
        h.set_root(p as usize);
        let a = ArenaHeap::attach(r.ptr).expect("attach");
        assert_eq!(a.root(), p as usize);
        assert_eq!(a.stats(), h.stats());
        let q = a.alloc(lay(100, 8));
        assert_ne!(p, q);
        *(r.ptr as *mut u64) = 0;
        assert!(ArenaHeap::attach(r.ptr).is_none());
    }
}

/// The wrapper's routing, over a local arena and the system allocator.
#[test]
fn routing_by_state_and_owner() {
    let r = Region::new(1 << 20);
    let h = r.heap();
    let inner = System;
    unsafe {
        // Armed: allocations come from the arena.
        let a = armed_alloc(h, &inner, lay(64, 8), false);
        assert!(h.owns(a));
        // Armed: a pre-P0 block is leaked, never handed to the inner allocator.
        let foreign = inner.alloc(lay(32, 8));
        routed_dealloc(ARMED, h, &inner, foreign, lay(32, 8));
        assert_eq!(h.stats().foreign_frees_while_armed, 1);
        // Armed realloc of a pre-P0 block moves it into the arena.
        let foreign2 = inner.alloc(lay(16, 8));
        fill(foreign2, 16, 9);
        let moved = routed_realloc(ARMED, h, &inner, foreign2, lay(16, 8), 48);
        assert!(h.owns(moved) && check(moved, 16, 9));
        assert_eq!(h.stats().foreign_frees_while_armed, 2);
        // Armed, arena full: inner allocator, counted.
        assert_eq!(h.stats().inner_allocs_while_armed, 0);
        let big = armed_alloc(h, &inner, lay(4 << 20, 8), false);
        assert!(!big.is_null() && !h.owns(big));
        assert_eq!(h.stats().inner_allocs_while_armed, 1);
        inner.dealloc(big, lay(4 << 20, 8));
        // After the window: arena frees are no-ops, the arena is unchanged.
        fill(a, 64, 5);
        let before = h.stats();
        let ign = IGNORED_FREES.load(Ordering::Relaxed);
        routed_dealloc(RESTORED, h, &inner, a, lay(64, 8));
        assert_eq!(h.stats(), before);
        assert!(IGNORED_FREES.load(Ordering::Relaxed) > ign);
        assert!(check(a, 64, 5));
        // ... and a realloc copies out to the inner allocator.
        let out = routed_realloc(DISARMED, h, &inner, a, lay(64, 8), 200);
        assert!(!h.owns(out) && check(out, 64, 5));
        assert_eq!(h.stats(), before);
        // A non-arena block after the window goes to the inner allocator.
        routed_dealloc(RESTORED, h, &inner, out, lay(200, 8));
    }
}

#[test]
fn transitions_refuse_outside_their_modes() {
    // image_layout is Off in this test process unless another test set it;
    // in Off, arm and adopt refuse without touching the state.
    if cratonvm_types::image_layout::mode() == cratonvm_types::image_layout::ImageMode::Off {
        assert_eq!(arm(), Err(ArenaError::WrongImageMode));
        assert_eq!(
            adopt_restored(ARENA_BASE, PAGE),
            Err(ArenaError::WrongImageMode)
        );
    }
    assert_eq!(disarm(), Err(ArenaError::WrongState));
    assert!(census().is_none());
    assert!(captured_extent().is_none());
}

/// Off-mode cost of the wrapper: an alloc/free loop through
/// `ImageArenaAlloc<System>` against `System` directly. Run with
/// `--ignored --nocapture` on an optimised build.
#[test]
#[ignore]
fn off_mode_overhead_bench() {
    fn run<A: GlobalAlloc>(a: &A) -> std::time::Duration {
        let t = std::time::Instant::now();
        let mut keep: [*mut u8; 64] = [std::ptr::null_mut(); 64];
        for i in 0..4_000_000usize {
            let l = lay(16 + (i % 7) * 24, 8);
            let k = i % 64;
            unsafe {
                if !keep[k].is_null() {
                    a.dealloc(keep[k], lay(16 + ((i - 64) % 7) * 24, 8));
                }
                keep[k] = std::hint::black_box(a.alloc(l));
            }
        }
        for (k, p) in keep.iter().enumerate() {
            let i = 4_000_000 - 64 + k;
            unsafe { a.dealloc(*p, lay(16 + (i % 7) * 24, 8)) };
        }
        t.elapsed()
    }
    let w = ImageArenaAlloc(System);
    let (mut best_s, mut best_w) = (u128::MAX, u128::MAX);
    for _ in 0..5 {
        best_s = best_s.min(run(&System).as_nanos());
        best_w = best_w.min(run(&w).as_nanos());
    }
    println!(
        "off-mode bench: System {} ms, ImageArenaAlloc<System> {} ms",
        best_s / 1_000_000,
        best_w / 1_000_000
    );
}

/// The "state lives in the arena" property: a parent builds an arena at a
/// fixed address, records its live blocks in the arena itself, and writes
/// the used bytes to a file; a child process maps that file MAP_PRIVATE at
/// the same address (plus a fresh reserve for the rest), attaches, checks
/// every block and keeps allocating, reallocating and freeing.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod linux_restore {
    use super::*;

    /// Away from ARENA_BASE so a test never collides with a real arena.
    const TEST_BASE: usize = 0x2400_0000_0000;
    const TEST_RESERVE: usize = 256 << 20;
    const ENV_FILE: &str = "CRATONVM_IMAGE_ARENA_TEST_CHILD_FILE";
    const OK_MARK: &str = "IMAGE_ARENA_CHILD_OK";

    /// One live block, as recorded in the arena.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rec {
        ptr: usize,
        size: usize,
        align: usize,
        seed: u64,
    }
    const N: usize = 2000;

    #[test]
    fn restored_bytes_at_the_same_address_keep_allocating() {
        let base = sys::reserve_fixed(TEST_BASE, TEST_RESERVE).expect("reserve test arena");
        let h = unsafe { ArenaHeap::init(base, TEST_RESERVE) };
        let mut rng = Rng(0xC0FF_EE00_1234_5678);
        unsafe {
            let recs = h.alloc(lay(N * std::mem::size_of::<Rec>(), 8)) as *mut Rec;
            for i in 0..N {
                let x = rng.next();
                let size = if i % 50 == 0 {
                    40_000 + (x as usize % 300_000)
                } else {
                    1 + (x as usize % 3000)
                };
                let align = 1usize << (x >> 50) % 7;
                let p = h.alloc(lay(size, align));
                fill(p, size, i as u64);
                *recs.add(i) = Rec {
                    ptr: p as usize,
                    size,
                    align,
                    seed: i as u64,
                };
            }
            // Free every third one so the free lists are populated at capture.
            for i in (0..N).step_by(3) {
                let r = *recs.add(i);
                h.dealloc(r.ptr as *mut u8, lay(r.size, r.align));
                (*recs.add(i)).ptr = 0;
            }
            h.set_root(recs as usize);
        }
        let used = h.used_len();
        let dir = std::env::temp_dir().join(format!("cvm-image-arena-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("arena.bin");
        let bytes = unsafe { std::slice::from_raw_parts(base, used) };
        std::fs::write(&file, bytes).unwrap();
        unsafe { sys::unmap(TEST_BASE, TEST_RESERVE) };

        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "image_arena::tests::linux_restore::child_entry",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_FILE, &file)
            .output()
            .expect("spawn child");
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "child failed: {}\n{stdout}\n{stderr}",
            out.status
        );
        assert!(
            stdout.contains(OK_MARK),
            "child did not run its body:\n{stdout}\n{stderr}"
        );
    }

    /// The child half; a no-op unless the parent set the env variable.
    #[test]
    fn child_entry() {
        let Some(file) = std::env::var_os(ENV_FILE) else {
            return;
        };
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(&file).expect("open image");
        let len = f.metadata().unwrap().len() as usize;
        assert!(len % PAGE == 0 && len >= HEADER_PAGE);
        sys::map_file_fixed(TEST_BASE, len, f.as_raw_fd()).expect("map image");
        drop(f);
        sys::reserve_fixed(TEST_BASE + len, TEST_RESERVE - len).expect("reserve tail");
        let h =
            unsafe { ArenaHeap::attach(TEST_BASE as *mut u8) }.expect("attach restored arena");
        assert_eq!(h.used_len(), len);
        let before = h.stats();
        let recs = h.root() as *mut Rec;
        assert!(h.owns(recs as *const u8));
        let mut rng = Rng(0x5EED);
        let mut verified = 0;
        unsafe {
            // Every live block survived.
            for i in 0..N {
                let r = *recs.add(i);
                if r.ptr != 0 {
                    assert!(
                        check(r.ptr as *const u8, r.size, r.seed),
                        "block {i} lost in restore"
                    );
                    verified += 1;
                }
            }
            // Keep going: new blocks (from the restored free lists and the
            // restored bump pointer), reallocs and frees, then recheck all.
            for i in 0..N {
                let r = *recs.add(i);
                let x = rng.next();
                let seed = (N + i) as u64;
                if r.ptr == 0 {
                    let size = 1 + (x as usize % 5000);
                    let p = h.alloc(lay(size, 8));
                    assert!(h.owns(p));
                    fill(p, size, seed);
                    *recs.add(i) = Rec {
                        ptr: p as usize,
                        size,
                        align: 8,
                        seed,
                    };
                } else if x % 4 == 0 {
                    let ns = r.size * 3 + 7;
                    let q = h.realloc(r.ptr as *mut u8, lay(r.size, r.align), ns);
                    assert!(check(q, r.size, r.seed));
                    fill(q, ns, seed);
                    *recs.add(i) = Rec {
                        ptr: q as usize,
                        size: ns,
                        align: r.align,
                        seed,
                    };
                } else if x % 4 == 1 {
                    h.dealloc(r.ptr as *mut u8, lay(r.size, r.align));
                    (*recs.add(i)).ptr = 0;
                }
            }
            for i in 0..N {
                let r = *recs.add(i);
                if r.ptr != 0 {
                    assert!(
                        check(r.ptr as *const u8, r.size, r.seed),
                        "block {i} clobbered after restore"
                    );
                }
            }
        }
        let after = h.stats();
        assert!(after.allocs > before.allocs && after.frees > before.frees);
        assert!(after.used_len >= before.used_len);
        println!(
            "{OK_MARK} verified={verified} allocs={} frees={}",
            after.allocs, after.frees
        );
    }
}
