    use super::*;

    // static int varargsCaller(int size) { return dropVarargs(size, new byte[size]); }
    //   0 iload_0; 1 iconst_1; 2 anewarray [B; 5 dup; 6 iconst_0; 7 iload_0;
    //   8 newarray byte; 10 aastore; 11 invokestatic
    const VARARGS: &[u8] = &[
        0x1a, 0x04, 0xbd, 0x00, 0x07, 0x59, 0x03, 0x1a, 0xbc, 0x08, 0x53, 0xb8, 0x00, 0x09, 0xac,
    ];
    // static int localCaller(int size) { byte[] a = new byte[size]; a[0] = 7; return drop(a, size); }
    //   0 iload_0; 1 newarray byte; 3 astore_1; 4 aload_1; 5 iconst_0; 6 bipush 7;
    //   8 bastore; 9 aload_1; 10 iload_0; 11 invokestatic
    const LOCAL: &[u8] = &[
        0x1a, 0xbc, 0x08, 0x4c, 0x2b, 0x03, 0x10, 0x07, 0x54, 0x2b, 0x1a, 0xb8, 0x00, 0x09, 0xac,
    ];

#[test]
    fn a_store_into_anything_older_or_any_other_commit_refuses() {
        // aload_0 (a parameter array); iconst_0; iconst_1; iastore; invokestatic
        let param = [0x2a, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x09, 0xb1];
        assert!(!prefix_stores_only_into_its_own_arrays(&param, param.len(), 4, &[]));
        // The local slot overwritten by a parameter before the store.
        // iload_0; newarray int; astore_1; aload_0; astore_1; aload_1; iconst_0;
        // iconst_1; iastore; invokestatic
        let overwritten =
            [0x1a, 0xbc, 0x0a, 0x4c, 0x2a, 0x4c, 0x2b, 0x03, 0x04, 0x4f, 0xb8, 0x00, 0x09, 0xb1];
        assert!(!prefix_stores_only_into_its_own_arrays(
            &overwritten,
            overwritten.len(),
            10,
            &[]
        ));
        // putstatic before the call.
        let putstatic = [0x01, 0xb3, 0x00, 0x05, 0xb8, 0x00, 0x09, 0xb1];
        assert!(!prefix_stores_only_into_its_own_arrays(&putstatic, putstatic.len(), 4, &[]));
        // A branch in the prefix: not modelled.
        // iload_0; ifeq +3; iload_0; newarray; ...
        let branch = [0x1a, 0x99, 0x00, 0x03, 0x1a, 0xbc, 0x08, 0xb8, 0x00, 0x09, 0xb1];
        assert!(!prefix_stores_only_into_its_own_arrays(&branch, branch.len(), 7, &[]));
        // A handler range that starts in the prefix.
        assert!(!prefix_stores_only_into_its_own_arrays(
            VARARGS,
            VARARGS.len(),
            11,
            &[(0, 11, 14)]
        ));
        // bci 0 and an out-of-range bci.
        assert!(!prefix_stores_only_into_its_own_arrays(VARARGS, VARARGS.len(), 0, &[]));
        assert!(!prefix_stores_only_into_its_own_arrays(VARARGS, VARARGS.len(), 99, &[]));
    }
