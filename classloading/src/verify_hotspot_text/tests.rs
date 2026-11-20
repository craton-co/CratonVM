    use super::*;

    fn method<'a>(code: &'a [u8]) -> HsMethod<'a> {
        HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code,
            exception_table: &[],
            stack_map_raw: None,
        }
    }

    #[test]
    fn bad_type_block_matches_hotspot_layout() {
        // iconst_0; invokestatic #7; return
        let code = [0x03, 0xb8, 0x00, 0x07, 0xb1];
        let pre = HsFrame {
            bci: 1,
            this_uninit: false,
            locals: vec![],
            stack: vec![HsType::V(VType::Int)],
        };
        let fault = VerifyFault::BadStackType {
            index: 0,
            actual: HsType::V(VType::Int),
            expected: HsType::V(VType::ObjectRef("java/lang/Object".into())),
        };
        let text = render_fault(&method(&code), 1, &fault, &pre);
        let expected = "Bad type on operand stack\n\
Exception Details:\n  \
Location:\n    \
p/C.m()V @1: invokestatic\n  \
Reason:\n    \
Type integer (current frame, stack[0]) is not assignable to 'java/lang/Object'\n  \
Current Frame:\n    \
bci: @1\n    \
flags: { }\n    \
locals: { }\n    \
stack: { integer }\n  \
Bytecode:\n    \
0000000: 03b8 0007 b1";
        assert!(text.starts_with(expected), "{text}");
        // The last line is padded to sixteen byte columns.
        let last = text.lines().last().unwrap();
        assert_eq!(last.len(), "    0000000:".len() + 8 * 5);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_sixteen_byte_method_gets_a_blank_padding_line() {
        let code = [0u8; 16];
        let mut out = String::new();
        bytecode_text(&mut out, &code);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("    0000010:"));
        assert!(lines[1][12..].chars().all(|c| c == ' '));
    }

    #[test]
    fn frames_name_long_halves_and_trim_padding() {
        let mut frame = VerificationFrame::initial_frame("p/C", "m", "(J)V", true, 4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        let f = HsFrame::current(&frame, 0);
        assert_eq!(f.locals, vec![HsType::V(VType::Long), HsType::Long2nd]);
        assert_eq!(f.stack, vec![HsType::V(VType::Long), HsType::Long2nd]);
        let mut out = String::new();
        frame_text(&mut out, &f);
        assert!(out.contains("locals: { long, long_2nd }"), "{out}");
    }

    #[test]
    fn stack_map_entries_print_absolute_offsets() {
        // two entries: same_frame delta 7 (@7), append_frame delta 2 (+Integer) (@10)
        let raw = [0x00, 0x02, 0x07, 252, 0x00, 0x02, 0x01];
        let mut out = String::new();
        stack_map_text(&mut out, &raw);
        assert_eq!(
            out,
            "  Stackmap Table:\n    same_frame(@7)\n    append_frame(@10,Integer)\n"
        );
    }
