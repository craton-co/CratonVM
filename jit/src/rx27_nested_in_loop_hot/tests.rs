    use super::*;

    /// `iload_1; ifeq 11; aload_0; invokevirtual #2; goto 4; return`: the
    /// call at pc 5 is in a loop.
    const LOOPING: [u8; 12] = [
        0x1b, 0x99, 0x00, 0x0a, 0x2a, 0xb6, 0x00, 0x02, 0xa7, 0xff, 0xfc, 0xb1,
    ];


    /// A `Matcher.group`-sized body (60 bytes) whose code holds a loop
    /// (`LOOPING`, padded).
    fn looping_body() -> InlineSite {
        let mut code = LOOPING.to_vec();
        code.resize(60, 0x00);
        InlineSite {
            callee_code_len: code.len(),
            callee_code: code,
            class_name: "java/util/regex/Matcher".to_string(),
            method_name: "group".to_string(),
            descriptor: "(I)Ljava/lang/String;".to_string(),
            ..InlineSite::default()
        }
    }


