pub(crate) const MAX_PENDING_INPUT: usize = 8 * 1024;

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
    pub(crate) fn push(&mut self, mut bytes: &[u8]) -> Vec<Key> {
        let mut keys = Vec::new();

        while !bytes.is_empty() {
            let available = MAX_PENDING_INPUT.saturating_sub(self.pending.len());
            if available == 0 {
                self.pending.clear();
                continue;
            }

            let appended = available.min(bytes.len());
            self.pending.extend_from_slice(&bytes[..appended]);
            bytes = &bytes[appended..];
            self.decode_pending(&mut keys);

            // The only input that can fill the pending buffer is an incomplete,
            // unterminated sequence. Drop it at the bound and resume parsing the
            // following bytes instead of retaining or growing it indefinitely.
            if self.pending.len() == MAX_PENDING_INPUT {
                self.pending.clear();
            }
        }

        debug_assert!(self.pending.len() <= MAX_PENDING_INPUT);
        keys
    }

    pub(crate) fn is_waiting_for_escape_key(&self) -> bool {
        self.pending == [0x1b]
    }

    pub(crate) fn expire_escape_key(&mut self) -> Option<Key> {
        if self.is_waiting_for_escape_key() {
            self.pending.clear();
            Some(Key::Back)
        } else {
            None
        }
    }

    fn decode_pending(&mut self, keys: &mut Vec<Key>) {
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
        0x00..=0x7f => Decode::Ignore(1),
        _ => decode_utf8(bytes),
    }
}

fn decode_escape(bytes: &[u8]) -> Decode {
    if bytes.len() == 1 {
        return Decode::Incomplete;
    }

    match bytes[1] {
        b'[' => decode_control_sequence(bytes),
        b'O' => decode_application_sequence(bytes),
        _ => Decode::Key(Key::Back, 1),
    }
}

fn decode_control_sequence(bytes: &[u8]) -> Decode {
    let Some(length) = escape_sequence_length(bytes, 2) else {
        return Decode::Incomplete;
    };
    let sequence = &bytes[..length];

    match sequence {
        b"\x1b[A" => Decode::Key(Key::Up, length),
        b"\x1b[B" => Decode::Key(Key::Down, length),
        b"\x1b[C" => Decode::Key(Key::Right, length),
        b"\x1b[D" => Decode::Key(Key::Left, length),
        b"\x1b[H" => Decode::Key(Key::First, length),
        b"\x1b[F" => Decode::Key(Key::Last, length),
        b"\x1b[5~" => Decode::Key(Key::PageUp, length),
        b"\x1b[6~" => Decode::Key(Key::PageDown, length),
        _ => Decode::Ignore(length),
    }
}

fn decode_application_sequence(bytes: &[u8]) -> Decode {
    let Some(length) = escape_sequence_length(bytes, 2) else {
        return Decode::Incomplete;
    };
    let sequence = &bytes[..length];

    match sequence {
        b"\x1bOA" => Decode::Key(Key::Up, length),
        b"\x1bOB" => Decode::Key(Key::Down, length),
        b"\x1bOC" => Decode::Key(Key::Right, length),
        b"\x1bOD" => Decode::Key(Key::Left, length),
        _ => Decode::Ignore(length),
    }
}

fn escape_sequence_length(bytes: &[u8], start: usize) -> Option<usize> {
    for (offset, byte) in bytes[start..].iter().copied().enumerate() {
        let index = start + offset;
        if (0x40..=0x7e).contains(&byte) {
            return Some(index + 1);
        }
        if !((0x20..=0x3f).contains(&byte)) {
            // Preserve the malformed byte so it can begin a fresh character or
            // escape sequence on the next decode pass.
            return Some(index);
        }
    }
    None
}

fn decode_utf8(bytes: &[u8]) -> Decode {
    let Some(length) = utf8_sequence_length(bytes[0]) else {
        return Decode::Ignore(1);
    };
    let available = bytes.len().min(length);

    match std::str::from_utf8(&bytes[..available]) {
        Ok(_) if available == length => Decode::Ignore(length),
        Ok(_) => Decode::Incomplete,
        Err(error) if error.error_len().is_none() && available < length => Decode::Incomplete,
        Err(error) => Decode::Ignore(error.error_len().unwrap_or(1)),
    }
}

fn utf8_sequence_length(first: u8) -> Option<usize> {
    match first {
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
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
    fn decodes_every_supported_escape_sequence_across_every_packet_boundary() {
        let sequences: &[(&[u8], Key)] = &[
            (b"\x1b[A", Key::Up),
            (b"\x1b[B", Key::Down),
            (b"\x1b[C", Key::Right),
            (b"\x1b[D", Key::Left),
            (b"\x1b[H", Key::First),
            (b"\x1b[F", Key::Last),
            (b"\x1b[5~", Key::PageUp),
            (b"\x1b[6~", Key::PageDown),
            (b"\x1bOA", Key::Up),
            (b"\x1bOB", Key::Down),
            (b"\x1bOC", Key::Right),
            (b"\x1bOD", Key::Left),
        ];

        for &(sequence, expected) in sequences {
            for boundary in 1..sequence.len() {
                let mut decoder = InputDecoder::default();
                assert!(
                    decoder.push(&sequence[..boundary]).is_empty(),
                    "decoded {sequence:?} before boundary {boundary}"
                );
                assert_eq!(
                    decoder.push(&sequence[boundary..]),
                    vec![expected],
                    "failed {sequence:?} at boundary {boundary}"
                );
            }
        }
    }

    #[test]
    fn retains_split_utf8_and_decodes_navigation_that_follows_it() {
        for character in ["é", "€", "🦀"] {
            let bytes = character.as_bytes();
            for boundary in 1..bytes.len() {
                let mut decoder = InputDecoder::default();
                assert!(decoder.push(&bytes[..boundary]).is_empty());
                let mut remainder = bytes[boundary..].to_vec();
                remainder.extend_from_slice(b"j\x1b[A");
                assert_eq!(decoder.push(&remainder), vec![Key::Down, Key::Up]);
            }
        }
    }

    #[test]
    fn malformed_and_overlong_input_recovers_at_the_pending_bound() {
        let mut decoder = InputDecoder::default();
        assert_eq!(
            decoder.push(&[0xc0, 0xaf, 0xf5, 0x80, 0xe2, b'j']),
            vec![Key::Down]
        );
        assert!(decoder.pending.is_empty());

        let mut unterminated = Vec::with_capacity(MAX_PENDING_INPUT);
        unterminated.extend_from_slice(b"\x1b[");
        unterminated.resize(MAX_PENDING_INPUT, b'1');
        assert!(decoder.push(&unterminated).is_empty());
        assert!(decoder.pending.is_empty());
        assert_eq!(decoder.push(b"j"), vec![Key::Down]);

        let oversized = vec![b'2'; MAX_PENDING_INPUT * 3];
        assert!(decoder.push(&oversized).is_empty());
        assert!(decoder.pending.len() <= MAX_PENDING_INPUT);
        assert_eq!(decoder.push(b"k"), vec![Key::Up]);
    }

    #[test]
    fn a_standalone_escape_becomes_back_only_when_expired() {
        let mut decoder = InputDecoder::default();

        assert!(decoder.push(b"\x1b").is_empty());
        assert!(decoder.is_waiting_for_escape_key());
        assert_eq!(decoder.expire_escape_key(), Some(Key::Back));
        assert!(!decoder.is_waiting_for_escape_key());
    }
}
