use super::*;
use std::sync::mpsc::channel;

/// Feed a character sequence through the paste state machine and collect
/// the keys it produced.
fn feed(chars: &str) -> Vec<Key> {
    let (tx, rx) = channel();
    let mut mode = PasteMode::Normal;
    for ch in chars.chars() {
        handle_paste_key(&mut mode, Some(ch), &tx);
    }
    rx.try_iter().collect()
}

#[test]
fn a_plain_character_is_left_alone() {
    let (tx, rx) = channel();
    let mut mode = PasteMode::Normal;
    assert_eq!(handle_paste_key(&mut mode, Some('a'), &tx), None);
    assert!(rx.try_iter().next().is_none());
}

#[test]
fn a_lone_escape_is_an_escape() {
    // Esc on its own must still abort the run: it is the only way out.
    // The machine waits for a possible sequence first, so the pending
    // Esc is flushed when the input goes quiet.
    let (tx, rx) = channel();
    let mut mode = PasteMode::Normal;
    handle_paste_key(&mut mode, Some('\u{1b}'), &tx);
    assert!(
        rx.try_iter().next().is_none(),
        "not yet: a sequence may follow"
    );
    flush_pending(&mut mode, &tx);
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Key::Esc]);
}

#[test]
fn a_pending_escape_is_not_flushed_twice() {
    let (tx, rx) = channel();
    let mut mode = PasteMode::Normal;
    handle_paste_key(&mut mode, Some('\u{1b}'), &tx);
    flush_pending(&mut mode, &tx);
    flush_pending(&mut mode, &tx);
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Key::Esc]);
}

#[test]
fn a_pasted_block_arrives_in_one_piece() {
    let keys = feed("\u{1b}[200~hello\u{1b}[201~");
    assert_eq!(keys, vec![Key::Paste("hello".to_string())]);
}

#[test]
fn a_paste_does_not_look_like_an_escape() {
    // The whole point: no Esc may escape from a paste.
    let keys = feed("\u{1b}[200~some text\u{1b}[201~");
    assert!(
        !keys.iter().any(|key| matches!(key, Key::Esc)),
        "{:?}",
        keys
    );
}

#[test]
fn a_multiline_paste_keeps_its_lines() {
    let keys = feed("\u{1b}[200~one\r\ntwo\u{1b}[201~");
    assert_eq!(keys, vec![Key::Paste("one\ntwo".to_string())]);
}

#[test]
fn an_unfinished_paste_stays_quiet() {
    let keys = feed("\u{1b}[200~half a paste");
    assert!(keys.is_empty(), "{:?}", keys);
}

#[test]
fn wheel_arrows_translate_into_scroll_keys() {
    // The wheel sends ESC [ A / ESC [ B; real arrow keys arrive as key
    // records, so these must scroll instead of paging through history.
    assert_eq!(feed("\u{1b}[A"), vec![Key::ScrollUp]);
    assert_eq!(feed("\u{1b}[B"), vec![Key::ScrollDown]);
    assert_eq!(feed("\u{1b}[C"), vec![Key::Right]);
    assert_eq!(feed("\u{1b}[D"), vec![Key::Left]);
}

#[test]
fn vt_style_home_and_page_keys_translate() {
    assert_eq!(feed("\u{1b}[5~"), vec![Key::PageUp]);
    assert_eq!(feed("\u{1b}[6~"), vec![Key::PageDown]);
    assert_eq!(feed("\u{1b}[3~"), vec![Key::Delete]);
}

#[test]
fn an_unknown_sequence_is_dropped_not_typed() {
    // A sequence we do not know must never leak characters into the
    // box.
    let keys = feed("\u{1b}[999;9X");
    assert!(keys.is_empty(), "{:?}", keys);
}

#[test]
fn a_paste_still_works_after_wheel_events() {
    let keys = feed("\u{1b}[A\u{1b}[B\u{1b}[200~text\u{1b}[201~");
    assert_eq!(
        keys,
        vec![
            Key::ScrollUp,
            Key::ScrollDown,
            Key::Paste("text".to_string())
        ]
    );
}

#[test]
fn the_escape_before_a_paste_is_not_a_second_escape() {
    // Two escapes in a row: the first one is a real Esc.
    let keys = feed("\u{1b}\u{1b}");
    assert_eq!(keys, vec![Key::Esc]);
}
