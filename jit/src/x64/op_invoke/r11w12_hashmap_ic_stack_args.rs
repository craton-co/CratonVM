use super::ic_stack_arg_block;
use crate::inline_cache_pic::ic_max_stack_words;

/// A site that fits the register file with the context word gets no
/// block, so its inline cache stays byte-identical to before.
#[test]
fn a_site_that_fits_the_registers_gets_no_block() {
    for n in 1..=3 {
        assert_eq!(ic_stack_arg_block(n, 4, true), None, "win64 n={n}");
    }
    for n in 1..=5 {
        assert_eq!(ic_stack_arg_block(n, 6, false), None, "sysv n={n}");
    }
}

/// `HashMap.newNode` (receiver + 4) on Win64: the context ABI needs two
/// stack words (arguments 3 and 4 of the Java list), the context-free ABI
/// one. 32 + 16 = 48, already 16-aligned; the words start above the home
/// area.
#[test]
fn hashmap_new_node_on_win64_gets_a_48_byte_block_above_the_home_area() {
    assert_eq!(ic_stack_arg_block(5, 4, true), Some((48, 32)));
    // n = 4: one context word overflows; 32 + 8 rounds to 48.
    assert_eq!(ic_stack_arg_block(4, 4, true), Some((48, 32)));
}

/// SysV has no home area; the block is the words, rounded to 16.
#[test]
fn sysv_blocks_have_no_home_area_and_stay_aligned() {
    assert_eq!(ic_stack_arg_block(6, 6, false), Some((16, 0)));
    assert_eq!(ic_stack_arg_block(7, 6, false), Some((16, 0)));
    assert_eq!(ic_stack_arg_block(8, 6, false), Some((32, 0)));
}

/// Every block is 16-aligned and holds every context-ABI stack word; past
/// the cap there is no block and the site keeps the helper.
#[test]
fn every_block_is_aligned_and_large_enough_and_the_cap_holds() {
    let cap = ic_max_stack_words();
    for (regs, win64) in [(4usize, true), (6, false)] {
        for n in 1..=(regs + cap + 2) {
            let words = (n + 1).saturating_sub(regs);
            match ic_stack_arg_block(n, regs, win64) {
                None => assert!(words == 0 || words > cap, "n={n}"),
                Some((total, base)) => {
                    assert_eq!(total % 16, 0, "n={n}");
                    assert!(total >= base + (words as i32) * 8, "n={n}");
                    assert_eq!(base, if win64 { 32 } else { 0 });
                }
            }
        }
    }
}
