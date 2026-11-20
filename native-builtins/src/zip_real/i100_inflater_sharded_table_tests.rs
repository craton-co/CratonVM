use super::*;
use crate::test_utils::mock_ctx;

/// An unsharded handle lives in shard 0 (the one table every VM with the
/// switch off shares, as before); a sharded one never does, and two
/// consecutive sharded handles land in different shards.
#[test]
fn i100_handles_find_their_shard_from_the_handle_alone() {
    let plain = inflater_init_in(true, true, false);
    let first = inflater_init_in(true, true, true);
    let second = inflater_init_in(true, true, true);
    assert_eq!(inflater_handle_shard(plain), 0);
    assert_eq!(plain & SHARDED_INFLATER_HANDLE_BIT, 0);
    for h in [first, second] {
        assert_ne!(h & SHARDED_INFLATER_HANDLE_BIT, 0);
        let shard = inflater_handle_shard(h);
        assert!((1..INFLATER_TABLE_SHARDS).contains(&shard), "{shard}");
        assert!(inflater_table(h).lock().unwrap().contains_key(&h));
    }
    // Other tests mint handles concurrently; adjacent ones always differ.
    if second - first < INFLATER_TABLE_SHARDS as i64 - 1 {
        assert_ne!(inflater_handle_shard(first), inflater_handle_shard(second));
    }
    assert!(inflater_table(plain).lock().unwrap().contains_key(&plain));
    let mut ctx = mock_ctx();
    for h in [plain, first, second] {
        infl_end(&mut ctx, &[Value::Long(h)]).unwrap();
        assert!(!inflater_table(h).lock().unwrap().contains_key(&h));
    }
}

/// A sharded stream inflates exactly as an unsharded one.
#[test]
fn i100_a_sharded_stream_inflates() {
    let payload: &[u8] = b"sharded sharded sharded inflater table table table";
    let mut c = Compress::new(flate2::Compression::default(), false);
    let mut compressed = Vec::with_capacity(256);
    c.compress_vec(payload, &mut compressed, FlushCompress::Finish)
        .expect("compress");
    let addr = inflater_init_in(true, true, true);
    let mut out = vec![0u8; 256];
    let step = infl_do_decompress(addr, &compressed, &mut out);
    assert!(step.finished);
    assert_eq!(&out[..step.output_consumed as usize], payload);
    let mut ctx = mock_ctx();
    infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
}
