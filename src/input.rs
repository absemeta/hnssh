#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
    Left,
    Right,
    Enter,
    Toggle,
    NextSibling,
    PreviousSibling,
    Parent,
    FirstChild,
    CollapseAll,
    ExpandAll,
    OpenPrimary,
    Back,
    Help,
    Refresh,
    Quit,
}

#[derive(Debug, Default)]
pub(crate) struct InputDecoder {
    pending: Vec<u8>,
}

impl InputDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut consumed = 0;

        while consumed < self.pending.len() {
            let remaining = &self.pending[consumed..];
            match decode_one(remaining) {
                Decode::Key(key, length) => {
                    keys.push(key);
                    consumed += length;
                }
                Decode::Ignore(length) => consumed += length,
                Decode::Incomplete => break,
            }
        }

        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        keys
    }
}

enum Decode {
    Key(Key, usize),
    Ignore(usize),
    Incomplete,
}

fn decode_one(bytes: &[u8]) -> Decode {
    let Some(&first) = bytes.first() else {
        return Decode::Incomplete;
    };

    match first {
        b'k' => Decode::Key(Key::Up, 1),
        b'j' => Decode::Key(Key::Down, 1),
        b'h' => Decode::Key(Key::Left, 1),
        b'l' => Decode::Key(Key::Right, 1),
        b'g' => Decode::Key(Key::First, 1),
        b'G' => Decode::Key(Key::Last, 1),
        b'n' => Decode::Key(Key::NextSibling, 1),
        b'p' => Decode::Key(Key::PreviousSibling, 1),
        b'P' => Decode::Key(Key::Parent, 1),
        b'N' => Decode::Key(Key::FirstChild, 1),
        b'C' => Decode::Key(Key::CollapseAll, 1),
        b'E' => Decode::Key(Key::ExpandAll, 1),
        b'o' => Decode::Key(Key::OpenPrimary, 1),
        b'?' => Decode::Key(Key::Help, 1),
        b'r' => Decode::Key(Key::Refresh, 1),
        b'q' | 0x03 => Decode::Key(Key::Quit, 1),
        b' ' => Decode::Key(Key::Toggle, 1),
        b'\r' | b'\n' => Decode::Key(Key::Enter, 1),
        0x08 | 0x7f => Decode::Key(Key::Back, 1),
        0x1b => decode_escape(bytes),
        _ => Decode::Ignore(utf8_sequence_length(first).min(bytes.len())),
    }
}

fn decode_escape(bytes: &[u8]) -> Decode {
    if bytes.len() == 1 {
        return Decode::Key(Key::Back, 1);
    }

    match bytes[1] {
        b'[' => decode_csi(bytes),
        b'O' => {
            if bytes.len() < 3 {
                Decode::Incomplete
            } else {
                match bytes[2] {
                    b'A' => Decode::Key(Key::Up, 3),
                    b'B' => Decode::Key(Key::Down, 3),
                    b'C' => Decode::Key(Key::Right, 3),
                    b'D' => Decode::Key(Key::Left, 3),
                    _ => Decode::Ignore(3),
                }
            }
        }
        _ => Decode::Key(Key::Back, 1),
    }
}

fn decode_csi(bytes: &[u8]) -> Decode {
    if bytes.len() < 3 {
        return Decode::Incomplete;
    }

    match bytes[2] {
        b'A' => Decode::Key(Key::Up, 3),
        b'B' => Decode::Key(Key::Down, 3),
        b'C' => Decode::Key(Key::Right, 3),
        b'D' => Decode::Key(Key::Left, 3),
        b'H' => Decode::Key(Key::First, 3),
        b'F' => Decode::Key(Key::Last, 3),
        b'5' | b'6' => {
            if bytes.len() < 4 {
                Decode::Incomplete
            } else if bytes[3] != b'~' {
                Decode::Ignore(4)
            } else if bytes[2] == b'5' {
                Decode::Key(Key::PageUp, 4)
            } else {
                Decode::Key(Key::PageDown, 4)
            }
        }
        _ => Decode::Ignore(3),
    }
}

fn utf8_sequence_length(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_coalesced_navigation_and_terminal_sequences() {
        let mut decoder = InputDecoder::default();

        assert_eq!(
            decoder.push(b"jk\x1b[A\x1b[C\x1b[D hlnpPNCEo\x1b[6~?\x03"),
            vec![
                Key::Down,
                Key::Up,
                Key::Up,
                Key::Right,
                Key::Left,
                Key::Toggle,
                Key::Left,
                Key::Right,
                Key::NextSibling,
                Key::PreviousSibling,
                Key::Parent,
                Key::FirstChild,
                Key::CollapseAll,
                Key::ExpandAll,
                Key::OpenPrimary,
                Key::PageDown,
                Key::Help,
                Key::Quit,
            ]
        );
    }

    #[test]
    fn retains_an_incomplete_csi_sequence() {
        let mut decoder = InputDecoder::default();

        assert!(decoder.push(b"\x1b[").is_empty());
        assert_eq!(decoder.push(b"5~"), vec![Key::PageUp]);
    }
}
