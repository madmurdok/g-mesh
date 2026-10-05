//! `CharColumns`: byte columns in, character columns out.

use g_mesh_plugin_sdk::wire::{Position, Range};
use g_mesh_plugin_sdk::CharColumns;

fn pos(line: u32, col: u32) -> Position {
    Position { line, col }
}

#[test]
fn columns_ascii_line_byte_and_char_columns_agree() {
    let columns = CharColumns::new("let x = 1;\nfoo();");
    assert_eq!(columns.at(0, 4), pos(0, 4));
    assert_eq!(columns.at(1, 3), pos(1, 3));
}

#[test]
fn columns_multibyte_characters_count_once() {
    // `é` and `ö` are two bytes each.
    let columns = CharColumns::new("héllo wörld");
    assert_eq!(columns.at(0, 3), pos(0, 2), "after `hé`");
    assert_eq!(columns.at(0, 10), pos(0, 8), "after `héllo wö`");
    assert_eq!(columns.at(0, 13), pos(0, 11), "end of line");
}

#[test]
fn columns_astral_character_counts_once() {
    // `😀` is four bytes and one character (two UTF-16 units).
    let columns = CharColumns::new("a😀b\n😀😀c");
    assert_eq!(columns.at(0, 5), pos(0, 2), "after `a😀`");
    assert_eq!(columns.at(1, 8), pos(1, 2), "after `😀😀` on the second line");
}

#[test]
fn columns_off_char_boundary_stays_a_byte_column() {
    let columns = CharColumns::new("hé😀");
    // Byte 2 is inside `é`, byte 5 inside `😀`.
    assert_eq!(columns.at(0, 2), pos(0, 2));
    assert_eq!(columns.at(0, 5), pos(0, 5));
}

#[test]
fn columns_past_the_last_line_or_its_end_stays_a_byte_column() {
    let columns = CharColumns::new("é\né");
    assert_eq!(columns.at(5, 7), pos(5, 7), "line past the end");
    assert_eq!(columns.at(1, 40), pos(1, 40), "byte column past the source's end");
}

#[test]
fn columns_range_converts_both_ends() {
    let columns = CharColumns::new("é = 1;\nfé()");
    assert_eq!(columns.range((0, 0), (1, 5)), Range { start: pos(0, 0), end: pos(1, 4) });
}

#[test]
fn columns_file_range_ends_at_newlines_and_last_line_chars() {
    let range = CharColumns::new("a\nbé😀").file_range();
    assert_eq!(range, Range { start: pos(0, 0), end: pos(1, 3) });
}

#[test]
fn columns_file_range_of_a_terminated_file_ends_on_the_empty_last_line() {
    assert_eq!(CharColumns::new("ab\ncd\n").file_range().end, pos(2, 0));
}

#[test]
fn columns_file_range_of_an_empty_file_is_the_origin() {
    assert_eq!(CharColumns::new("").file_range(), Range { start: pos(0, 0), end: pos(0, 0) });
}

#[test]
fn columns_carriage_return_is_an_ordinary_character() {
    let columns = CharColumns::new("ab\r\ncd\r");
    assert_eq!(columns.file_range().end, pos(1, 3));
    assert_eq!(columns.at(1, 2), pos(1, 2));
}
