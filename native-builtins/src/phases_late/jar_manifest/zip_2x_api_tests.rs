#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::{Cursor, Read, Write};

/// Round-trip a two-entry ZIP through the 2.x writer + reader so a
/// future accidental revert to 0.6 (or an API drift in 2.x) trips a
/// dedicated, scoped test rather than only being caught by one of
/// the surrounding integration tests.
#[test]
fn round_trip_simple_zip_two_entries() {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("a.txt", opts).expect("start a.txt");
        zw.write_all(b"alpha").unwrap();
        zw.start_file("dir/b.bin", opts).expect("start dir/b.bin");
        zw.write_all(&[0u8, 1, 2, 3, 4, 5]).unwrap();
        zw.finish().expect("finish zip");
    }

    let inner = buf.into_inner();
    let mut archive =
        zip::ZipArchive::new(Cursor::new(inner)).expect("re-open round-tripped zip");
    assert_eq!(archive.len(), 2, "expected two entries");

    let mut s = String::new();
    archive
        .by_name("a.txt")
        .expect("a.txt entry")
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "alpha");

    let mut bin = Vec::new();
    archive
        .by_name("dir/b.bin")
        .expect("b.bin entry")
        .read_to_end(&mut bin)
        .unwrap();
    assert_eq!(bin, vec![0u8, 1, 2, 3, 4, 5]);
}

/// `SimpleFileOptions::default()` must surface a ZIP that the
/// reader resolves as the conventional "deflate" method when an
/// entry is added with the explicit `.compression_method(Deflated)`
/// builder call — and `Stored` when configured for no compression.
/// Callers across the crate rely on these two methods round-tripping
/// to method codes 8 / 0 respectively (the SecurityManager
/// signed-JAR fixtures store entries to keep digests byte-stable;
/// the ZipOutputStream production path deflates).
#[test]
fn simple_file_options_compression_methods() {
    for (label, method, expected_code) in &[
        ("stored", zip::CompressionMethod::Stored, 0u16),
        ("deflated", zip::CompressionMethod::Deflated, 8u16),
    ] {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(*method);
            zw.start_file(format!("{label}.dat"), opts).unwrap();
            zw.write_all(label.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let bytes = buf.into_inner();
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).expect("re-open per-method zip");
        let entry = archive
            .by_name(&format!("{label}.dat"))
            .expect("named entry present");
        // `.compression()` returns the configured `CompressionMethod`
        // — compare via the standard method code so this stays a
        // black-box assertion independent of 2.x enum layout.
        let got: u16 = match entry.compression() {
            zip::CompressionMethod::Stored => 0,
            zip::CompressionMethod::Deflated => 8,
            other => panic!("unexpected method {other:?} for {label}"),
        };
        assert_eq!(got, *expected_code, "method code mismatch for {label}");
    }
}
