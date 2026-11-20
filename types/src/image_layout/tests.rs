use super::*;

#[test]
fn mode_defaults_off_and_moves_forward_only() {
    assert_eq!(mode(), ImageMode::Off);
    assert!(!capturing());
    set_mode(ImageMode::Capture);
    assert!(capturing());
    set_mode(ImageMode::Restored);
    assert_eq!(mode(), ImageMode::Restored);
    let back = std::panic::catch_unwind(|| set_mode(ImageMode::Capture));
    assert!(back.is_err(), "Restored -> Capture must be refused");
}

#[cfg(target_os = "linux")]
#[test]
fn the_census_lists_a_named_thread_and_the_capture_check_names_it() {
    let (release, parked) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready) = std::sync::mpsc::channel::<u64>();
    let child = std::thread::Builder::new()
        .name("wp3-census-pr".to_string())
        .spawn(move || {
            ready_tx.send(current_os_tid().unwrap()).unwrap();
            let _ = parked.recv();
        })
        .unwrap();
    let child_tid = ready.recv().unwrap();
    let census = os_thread_census().expect("linux has /proc/self/task");
    assert!(census.iter().any(|t| t.tid == child_tid && t.name == "wp3-census-pr"));
    let me = current_os_tid().unwrap();
    let blocking = threads_blocking_capture(&[me]).unwrap();
    assert!(blocking.iter().any(|t| t.tid == child_tid), "{blocking:?}");
    assert!(blocking.iter().all(|t| t.tid != me));
    let blocking = threads_blocking_capture(&[me, child_tid]).unwrap();
    assert!(blocking.iter().all(|t| t.tid != child_tid));
    release.send(()).unwrap();
    child.join().unwrap();
}

#[cfg(not(target_os = "linux"))]
#[test]
fn no_census_off_linux() {
    assert!(os_thread_census().is_none());
    assert!(threads_blocking_capture(&[]).is_none());
}

/// One boot's worth of requests, as the sites make them.
fn boot_sequence(b: &Broker) -> Vec<Option<usize>> {
    vec![
        b.place(SegmentKind::Heap, 64 << 20, 2 << 20),
        b.place(SegmentKind::GcSideTable, 1 << 20, 0),
        b.place(SegmentKind::Heap, 3 << 20, 2 << 20),
        b.place(SegmentKind::JitCode, 64 * 1024, 0),
        b.place(SegmentKind::PollPage, 2 * PAGE, 0),
        b.place(SegmentKind::ClassPathArchive, 12_345, 0),
        b.place(SegmentKind::RuntimeImage, 146 << 20, 0),
        b.place(SegmentKind::JimageDirect, 146 << 20, 0),
        b.place(SegmentKind::JitCode, 32 << 20, 0),
        b.place(SegmentKind::ClassPathArchive, 1, 0),
    ]
}

#[test]
fn placement_is_deterministic_aligned_and_disjoint() {
    let first = boot_sequence(&Broker::new());
    let second = boot_sequence(&Broker::new());
    assert_eq!(first, second, "a fresh broker must replay the same addresses");
    let addrs: Vec<usize> = first.iter().map(|a| a.expect("placed")).collect();
    // Heap reservations land on 2 MiB boundaries, everything on a page.
    assert_eq!(addrs[0] % (2 << 20), 0);
    assert_eq!(addrs[2] % (2 << 20), 0);
    assert!(addrs.iter().all(|a| a % PAGE == 0));
    // The first of each kind is its region base; later ones follow.
    assert_eq!(addrs[0], SegmentKind::Heap.region().0);
    assert_eq!(addrs[2], addrs[0] + (64 << 20));
    assert_eq!(addrs[8], addrs[3] + 64 * 1024);
    assert_eq!(addrs[9], addrs[5] + 12_345usize.next_multiple_of(PAGE));
    // A kind's later request does not move another kind's addresses.
    let b = Broker::new();
    b.place(SegmentKind::Heap, 1 << 30, 0).unwrap();
    assert_eq!(
        b.place(SegmentKind::JitCode, 64 * 1024, 0),
        Some(addrs[3]),
        "regions are per kind"
    );
}

#[test]
fn regions_are_disjoint_and_clear_of_the_arena_the_pie_base_and_2gib() {
    let mut spans: Vec<(usize, usize)> = SegmentKind::ALL.iter().map(|k| k.region()).collect();
    spans.sort();
    for w in spans.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "regions overlap: {w:?}");
    }
    for k in SegmentKind::ALL {
        let (base, size) = k.region();
        assert_eq!(base % (2 << 20), 0, "{k:?} base not 2 MiB aligned");
        if k == SegmentKind::PollPage {
            assert!(base + size <= 0x7FFF_0000, "poll pages must end below 2 GiB");
        } else {
            assert!(base >= CAPTURE_ARENA_BASE + (8 << 40), "{k:?} too close to the arena");
            assert!(base + size <= 0x5555_0000_0000, "{k:?} reaches the PIE base");
        }
    }
}

#[test]
fn a_full_region_and_an_empty_request_are_refused() {
    let b = Broker::new();
    let (_, size) = SegmentKind::PollPage.region();
    assert_eq!(b.place(SegmentKind::PollPage, 0, 0), None);
    assert!(b.place(SegmentKind::PollPage, size, 0).is_some());
    assert_eq!(b.place(SegmentKind::PollPage, PAGE, 0), None, "region is full");
    assert_eq!(b.used(SegmentKind::PollPage), size);
    assert_eq!(b.place(SegmentKind::PollPage, usize::MAX, 0), None, "no overflow");
}

#[test]
fn concurrent_placements_never_overlap() {
    let b = std::sync::Arc::new(Broker::new());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let b = b.clone();
            std::thread::spawn(move || {
                (0..200)
                    .map(|_| b.place(SegmentKind::JitCode, 3 * PAGE, 0).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut all: Vec<usize> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
    all.sort();
    for w in all.windows(2) {
        assert!(w[1] - w[0] >= 3 * PAGE);
    }
}

#[test]
fn registry_records_forgets_and_retags() {
    let r = Registry::new();
    let anon = |addr| Segment {
        kind: SegmentKind::Heap,
        addr,
        len: 2 << 20,
        prot: 0,
        backing: Backing::Anonymous { hugetlb: false },
    };
    r.record(anon(0x3000_0000_0000));
    r.record(anon(0x2800_0000_0000));
    let file = Segment {
        kind: SegmentKind::RuntimeImage,
        addr: 0x4800_0000_0000,
        len: PAGE,
        prot: 1,
        backing: Backing::File(FileIdentity {
            path: "modules".into(),
            offset: 0,
            shared: true,
            dev: 1,
            ino: 2,
            size: 3,
            mtime_sec: 4,
            mtime_nsec: 5,
        }),
    };
    r.record(file.clone());
    let addrs: Vec<usize> = r.segments().iter().map(|s| s.addr).collect();
    assert_eq!(addrs, [0x2800_0000_0000, 0x3000_0000_0000, 0x4800_0000_0000]);
    assert!(r.retag_hugetlb(0x2800_0000_0000, true));
    assert_eq!(
        r.segments()[0].backing,
        Backing::Anonymous { hugetlb: true }
    );
    assert!(!r.retag_hugetlb(0x4800_0000_0000, true), "a file is never hugetlb");
    assert_eq!(r.forget(0x3000_0000_0000).map(|s| s.addr), Some(0x3000_0000_0000));
    assert_eq!(r.forget(0x3000_0000_0000), None);
    assert_eq!(r.segments().len(), 2);
    assert_eq!(r.segments()[1], file);
}

#[test]
fn remap_without_file_segments_maps_nothing() {
    let anon = Segment {
        kind: SegmentKind::Heap,
        addr: 0x2800_0000_0000,
        len: PAGE,
        prot: 0,
        backing: Backing::Anonymous { hugetlb: false },
    };
    assert_eq!(remap_file_segments(&[anon]).unwrap(), 0);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::super::*;

    fn temp_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cratonvm_image_layout_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("modules");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn unmap(addr: usize, len: usize) {
        // SAFETY: a test-owned mapping at `addr`.
        unsafe { sys::munmap(addr as *mut u8, len) };
    }

    /// The capture/restore pair end to end: a file mapped at a brokered
    /// address is recorded with its identity, and after it is unmapped
    /// the restore helper puts the same bytes back at the same address.
    #[test]
    fn a_captured_file_mapping_remaps_at_the_same_address() {
        let bytes: Vec<u8> = (0..3 * PAGE as u32 + 17).map(|i| (i % 251) as u8).collect();
        let path = temp_file("remap", &bytes);
        let broker = Broker::new();
        let registry = Registry::new();
        let file = std::fs::File::open(&path).unwrap();
        let mapping =
            map_file_readonly_in(&broker, &registry, SegmentKind::RuntimeImage, &file, &path)
                .expect("mapped at the brokered address");
        let addr = mapping.as_ptr() as usize;
        assert_eq!(addr, SegmentKind::RuntimeImage.region().0);
        assert_eq!(&mapping[..], &bytes[..]);
        let segs = registry.segments();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].addr, addr);
        assert_eq!(segs[0].len, bytes.len().next_multiple_of(PAGE));
        let Backing::File(id) = &segs[0].backing else {
            panic!("file-backed")
        };
        assert_eq!(id.size, bytes.len() as u64);
        assert!(id.shared);
        drop(mapping);
        drop(file);

        assert_eq!(remap_file_segments(&segs).expect("same file"), 1);
        // SAFETY: just mapped back, `bytes.len()` readable bytes.
        let seen = unsafe { std::slice::from_raw_parts(addr as *const u8, bytes.len()) };
        assert_eq!(seen, &bytes[..]);
        unmap(addr, segs[0].len);

        // A file that changed is refused before anything is mapped.
        std::fs::write(&path, &bytes[..PAGE]).unwrap();
        match remap_file_segments(&segs) {
            Err(RemapError::Changed { field, .. }) => assert_eq!(field, "size"),
            other => panic!("expected Changed, got {other:?}"),
        }
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            remap_file_segments(&segs),
            Err(RemapError::Open { .. })
        ));
    }

    /// An anonymous capture mapping lands at the brokered address and is
    /// recorded; a second request for an occupied address is refused
    /// rather than mapped over (`MAP_FIXED_NOREPLACE`).
    #[test]
    fn an_anonymous_capture_mapping_is_placed_recorded_and_never_clobbers() {
        const PROT_READ_WRITE: i32 = 0x3;
        const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;
        let broker = Broker::new();
        let registry = Registry::new();
        let p = map_anonymous_in(
            &broker,
            &registry,
            SegmentKind::GcSideTable,
            5 * PAGE,
            0,
            PROT_READ_WRITE,
            MAP_PRIVATE_ANON,
        )
        .expect("placed");
        assert_eq!(p as usize, SegmentKind::GcSideTable.region().0);
        // SAFETY: a fresh read-write mapping of 5 pages.
        unsafe {
            assert_eq!(*p.add(4 * PAGE), 0);
            *p.add(4 * PAGE) = 7;
        }
        let segs = registry.segments();
        assert_eq!(segs.len(), 1);
        assert_eq!(
            (segs[0].addr, segs[0].len, segs[0].prot),
            (p as usize, 5 * PAGE, PROT_READ_WRITE)
        );
        // A second broker replays the same address, which is now taken.
        let again = map_anonymous_in(
            &Broker::new(),
            &registry,
            SegmentKind::GcSideTable,
            PAGE,
            0,
            PROT_READ_WRITE,
            MAP_PRIVATE_ANON,
        );
        assert!(again.is_none(), "an occupied address must be refused");
        // SAFETY: the mapping above is still intact.
        assert_eq!(unsafe { *p.add(4 * PAGE) }, 7);
        assert!(spoiled().is_some());
        unmap(p as usize, 5 * PAGE);
    }
}
