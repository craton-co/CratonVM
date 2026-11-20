    //! JIT round 19, lane mega19: the single-pass twins of the round-18 mega
    //! switches (MEGA5-18-1), the register-probe miss to the helper
    //! (MEGA5-18-3) and the quiet inline publication (MEGA5-18-4). The emitters
    //! these switches pick are the round-18 ones; what is pinned here is that
    //! every switch is opt-in, that the located probe is the probe byte for
    //! byte and names every baked slot, and that the located lean stub without
    //! an identity is the located stub byte for byte.
    use super::*;

    const PIC: usize = 0x7fff_0000_0000_2000;



    /// JIT round 19 wave 4 (lane megaosr4): MEGA2-4 and MEGA2-3 are default on,
    /// read live, and independent of MEGA19-2's gate switch.
/// JIT round 19 wave 2 (lane mega2): MEGA19-2 and MEGA19-4 are default on, and
    /// MEGA19-4 answers only for a slot that is megamorphic at emission, never
    /// for a null slot (which it must not read).
/// The located probe is [`emit_mega_register_probe`] byte for byte, and
    /// every offset it reports holds the baked slot address (so a single-pass
    /// loop copy re-points all of them).
    #[test]
    fn the_located_register_probe_names_every_baked_slot() {
        for cell_first in [true, false] {
            let mut plain = ExecutableBuffer::new(1024).expect("buffer");
            let plain_exits = emit_mega_register_probe(&mut plain, PIC, cell_first);
            let mut located = ExecutableBuffer::new(1024).expect("buffer");
            let mut imm_ats = Vec::new();
            let exits =
                emit_mega_register_probe_located(&mut located, PIC, cell_first, &mut imm_ats);
            assert_eq!(plain.as_slice(), located.as_slice());
            assert_eq!(plain_exits, exits);
            assert_eq!(imm_ats.len(), 1, "the probe re-bakes the slot once");
            let bytes = located.as_slice();
            for at in &imm_ats {
                let word = u64::from_le_bytes(bytes[*at..*at + 8].try_into().expect("8 bytes"));
                assert_eq!(word, PIC as u64);
            }
            // No other copy of the address hides in the probe.
            let needle = (PIC as u64).to_le_bytes();
            let all = bytes.windows(8).filter(|w| *w == needle).count();
            assert_eq!(all, imm_ats.len());
        }
    }

    /// Without an identity the single-pass lean entry point emits exactly the
    /// located stub (both gate-entry shapes) and returns no lean exit.
    #[test]
    fn the_located_lean_stub_without_an_identity_is_the_located_stub() {
        crate::x64::set_moving_young_override(Some(false));
        for fast in [false, true] {
            let mut a = ExecutableBuffer::new(4096).expect("buffer");
            let (hits_a, imm_a, fast_a) =
                emit_hashed_vtable_stub_located(&mut a, PIC, 24, &[32, 40], 0, 0, 0, 32, fast);
            let mut b = ExecutableBuffer::new(4096).expect("buffer");
            let (hits_b, imm_b, fast_b, lean) = emit_hashed_vtable_stub_located_lean(
                &mut b,
                PIC,
                24,
                &[32, 40],
                0,
                0,
                0,
                32,
                fast,
                None,
                None,
            );
            assert_eq!(a.as_slice(), b.as_slice());
            assert_eq!((hits_a, imm_a, fast_a), (hits_b, imm_b, fast_b));
            assert!(lean.is_empty());
        }
    }

    /// JIT round 21 wave 13 (lane trace,
    /// `CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY`): a service identity without
    /// the lean protocol leaves the hit protocol alone (no lean exit) and adds
    /// exactly one republish of the caller's id, on the sentinel's cold side
    /// (after the `CMP RAX, 1`), ahead of the service call. A shape without
    /// the inline mirrors ignores it byte for byte. Bytes only: the stub is
    /// never run, so the helper, record and info words are stand-ins.
    #[test]
    fn the_stub_service_republishes_the_callers_identity_without_the_lean_protocol() {
        crate::x64::set_moving_young_override(Some(false));
        const ID: u32 = 0x5EED_1D21;
        const FRAME_RECORD: usize = 0x7fff_0000_0001_0000;
        const SERVICE: usize = 0x7fff_0000_0002_0000;
        const INFO: usize = 0x7fff_0000_0003_0000;
        for fast in [false, true] {
            let emit = |identity: Option<u32>| {
                let mut buf = ExecutableBuffer::new(8192).expect("buffer");
                let (hits, _, _, lean) = emit_hashed_vtable_stub_located_lean(
                    &mut buf,
                    PIC,
                    24,
                    &[32, 40],
                    FRAME_RECORD,
                    SERVICE,
                    INFO,
                    32,
                    fast,
                    None,
                    identity,
                );
                assert!(lean.is_empty(), "the hit protocol is unchanged");
                (buf.as_slice().to_vec(), hits.len())
            };
            let (plain, plain_hits) = emit(None);
            let (named, named_hits) = emit(Some(ID));
            assert_eq!(plain_hits, named_hits);
            let id_bytes = ID.to_le_bytes();
            let id_at = |b: &[u8]| b.windows(4).position(|w| w == id_bytes);
            assert_eq!(id_at(&plain), None, "no identity unless asked for");
            if lean_stub_hit_shape(FRAME_RECORD) {
                let at = id_at(&named).expect("the cold service names the caller");
                let cmp = [0x48, 0x83, 0xF8, 0x01];
                assert!(
                    named[..at].windows(4).any(|w| w == cmp),
                    "the identity store is on the sentinel's cold side"
                );
                // One `MOV <seg>:[disp32], RBP` (9 bytes) and one
                // `MOV dword <seg>:[disp32], imm32` (12 bytes).
                assert_eq!(named.len(), plain.len() + 21);
                assert_eq!(named.windows(4).filter(|w| *w == id_bytes).count(), 1);
            } else {
                assert_eq!(plain, named, "no inline mirrors: the identity is ignored");
            }
        }
    }
