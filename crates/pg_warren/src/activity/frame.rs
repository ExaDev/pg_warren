//! A backend's progress, as its slot holds it (DESIGN.md §5.7): two
//! frames, the *command* (the outermost long call of the statement, set by
//! `warren.report_progress`) and the *operation* (the native call running
//! now). Each holds a name, graph, version, phase, units done of a total
//! and a start time; the operation also holds a few named counters.
//! Text is kept in fixed widths, cut at a character boundary.

/// Text of at most `BYTES` bytes, in `BYTES / 8` words; empty is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Text<const BYTES: usize>([u8; BYTES]);

impl<const BYTES: usize> Text<BYTES> {
    pub(crate) const EMPTY: Self = Self([0; BYTES]);
    const WORDS: usize = BYTES / 8;

    /// `text`, cut to its longest prefix of at most `BYTES` bytes that ends
    /// at a character boundary.
    pub(crate) fn new(text: &str) -> Self {
        let cut = &text[..text.floor_char_boundary(BYTES)];
        let mut bytes = [0; BYTES];
        bytes[..cut.len()].copy_from_slice(cut.as_bytes());
        Self(bytes)
    }

    /// The text.
    pub(crate) fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&byte| byte == 0).unwrap_or(BYTES);
        // Written from a `str` cut at a character boundary, or from words
        // a torn read would have retried; anything else shows as empty.
        std::str::from_utf8(&self.0[..len]).unwrap_or_default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0[0] == 0
    }

    fn encode(&self, words: &mut [u64]) {
        for (word, bytes) in words.iter_mut().zip(self.0.chunks_exact(8)) {
            *word = u64::from_le_bytes(bytes.try_into().unwrap_or_default());
        }
    }

    fn decode(words: &[u64]) -> Self {
        let mut bytes = [0; BYTES];
        for (bytes, word) in bytes.chunks_exact_mut(8).zip(words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        Self(bytes)
    }
}

/// `value` as a word, `NONE` if absent.
fn optional(value: Option<u64>) -> u64 {
    value.unwrap_or(NONE)
}

/// A word as an optional value.
fn present(word: u64) -> Option<u64> {
    (word != NONE).then_some(word)
}

/// A name or phase.
pub(crate) type Name = Text<32>;
/// A counter's name.
pub(crate) type DetailName = Text<16>;

/// A frame: none if its name is empty.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Frame {
    pub(crate) name: Name,
    pub(crate) graph: Option<i32>,
    pub(crate) version: Option<i64>,
    pub(crate) phase: Name,
    pub(crate) done: Option<u64>,
    pub(crate) total: Option<u64>,
    /// When it started, as a `timestamptz`.
    pub(crate) start: i64,
}

/// The words a [`Frame`] takes.
pub(super) const FRAME_WORDS: usize = 2 * Name::WORDS + 5;

/// The counters an operation keeps.
pub(crate) const DETAILS: usize = 3;

/// An operation's frame and its named counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Operation {
    pub(crate) frame: Frame,
    pub(crate) details: [(DetailName, f64); DETAILS],
}

/// The words an [`Operation`] takes.
pub(super) const OPERATION_WORDS: usize = FRAME_WORDS + DETAILS * (DetailName::WORDS + 1);

/// An absent `Option<u64>`.
const NONE: u64 = u64::MAX;

impl Frame {
    pub(crate) const NONE: Self = Self {
        name: Name::EMPTY,
        graph: None,
        version: None,
        phase: Name::EMPTY,
        done: None,
        total: None,
        start: 0,
    };

    /// Whether it is a frame.
    pub(crate) fn is_some(&self) -> bool {
        !self.name.is_empty()
    }

    /// Starts phase `phase`, of `total` units if known, with none done.
    pub(crate) fn enter(&mut self, phase: &str, total: Option<u64>) {
        self.phase = Name::new(phase);
        self.done = Some(0);
        self.total = total;
    }

    pub(super) fn encode(&self, words: &mut [u64]) {
        let (name, rest) = words.split_at_mut(Name::WORDS);
        let (phase, rest) = rest.split_at_mut(Name::WORDS);
        self.name.encode(name);
        self.phase.encode(phase);
        rest.copy_from_slice(&[
            optional(self.graph.map(|graph| u64::from(graph as u32))),
            optional(self.version.map(|version| version as u64)),
            optional(self.done),
            optional(self.total),
            self.start as u64,
        ]);
    }

    pub(super) fn decode(words: &[u64]) -> Self {
        let name = Name::decode(&words[..Name::WORDS]);
        if name.is_empty() {
            return Self::NONE;
        }
        let rest = &words[2 * Name::WORDS..];
        Self {
            name,
            phase: Name::decode(&words[Name::WORDS..2 * Name::WORDS]),
            graph: present(rest[0]).map(|graph| graph as u32 as i32),
            version: present(rest[1]).map(|version| version as i64),
            done: present(rest[2]),
            total: present(rest[3]),
            start: rest[4] as i64,
        }
    }
}

impl Operation {
    pub(crate) const NONE: Self =
        Self { frame: Frame::NONE, details: [(DetailName::EMPTY, 0.0); DETAILS] };

    /// Starts phase `phase` of the operation, clearing its counters.
    pub(crate) fn enter(&mut self, phase: &str, total: Option<u64>) {
        self.frame.enter(phase, total);
        self.details = Self::NONE.details;
    }

    /// Sets the counter `name` to `value`: the one of that name, or the
    /// first free; ignored if all are taken by others.
    pub(crate) fn set_detail(&mut self, name: &str, value: f64) {
        let name = DetailName::new(name);
        let slot = self.details.iter().position(|(held, _)| *held == name);
        let slot = slot.or_else(|| self.details.iter().position(|(held, _)| held.is_empty()));
        if let Some(slot) = slot {
            self.details[slot] = (name, value);
        }
    }

    /// The counters set, in order.
    pub(crate) fn details(&self) -> impl Iterator<Item = (&str, f64)> {
        self.details
            .iter()
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, value)| (name.as_str(), *value))
    }

    pub(super) fn encode(&self, words: &mut [u64]) {
        let (frame, mut rest) = words.split_at_mut(FRAME_WORDS);
        self.frame.encode(frame);
        for (name, value) in &self.details {
            let (detail, after) = rest.split_at_mut(DetailName::WORDS + 1);
            name.encode(&mut detail[..DetailName::WORDS]);
            detail[DetailName::WORDS] = value.to_bits();
            rest = after;
        }
    }

    pub(super) fn decode(words: &[u64]) -> Self {
        let frame = Frame::decode(&words[..FRAME_WORDS]);
        if !frame.is_some() {
            return Self::NONE;
        }
        let mut details = Self::NONE.details;
        let stored = words[FRAME_WORDS..].chunks_exact(DetailName::WORDS + 1);
        for (detail, words) in details.iter_mut().zip(stored) {
            let name = DetailName::decode(&words[..DetailName::WORDS]);
            *detail = (name, f64::from_bits(words[DetailName::WORDS]));
        }
        Self { frame, details }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(Name::new("louvain").as_str(), "louvain");
        assert_eq!(Name::new("").as_str(), "");
        assert!(Name::new("").is_empty());
        let long = "a".repeat(31) + "é";
        assert_eq!(Name::new(&long).as_str(), "a".repeat(31), "é would end past byte 32");
        assert_eq!(Name::new(&"b".repeat(40)).as_str(), "b".repeat(32));
        assert_eq!(DetailName::new("converged").as_str(), "converged");
    }

    fn frame() -> Frame {
        Frame {
            name: Name::new("refresh"),
            graph: Some(7),
            version: Some(12),
            phase: Name::new("reading edges"),
            done: Some(5),
            total: Some(9),
            start: 812_345_678,
        }
    }

    #[test]
    fn frames_round_trip_through_their_words() {
        for frame in [frame(), Frame::NONE, Frame { graph: None, total: None, ..frame() }] {
            let mut words = [0; FRAME_WORDS];
            frame.encode(&mut words);
            assert_eq!(Frame::decode(&words), frame);
        }
        let mut operation = Operation { frame: frame(), ..Operation::NONE };
        operation.set_detail("moved", 3.5);
        let mut words = [0; OPERATION_WORDS];
        operation.encode(&mut words);
        assert_eq!(Operation::decode(&words), operation);
        assert!(!Frame::NONE.is_some() && frame().is_some());
        assert_eq!(Frame::decode(&[0; FRAME_WORDS]), Frame::NONE, "zeroed words are no frame");
        assert_eq!(Operation::decode(&[0; OPERATION_WORDS]), Operation::NONE);
    }

    #[test]
    fn a_phase_starts_with_none_done_and_clears_the_counters() {
        let mut operation = Operation { frame: frame(), ..Operation::NONE };
        operation.set_detail("restarts", 2.0);
        operation.enter("lanczos", Some(100));
        assert_eq!(operation.frame.phase.as_str(), "lanczos");
        assert_eq!((operation.frame.done, operation.frame.total), (Some(0), Some(100)));
        assert_eq!(operation.details().count(), 0);
    }

    #[test]
    fn counters_are_set_by_name_while_there_is_room() {
        let mut operation = Operation::NONE;
        operation.set_detail("a", 1.0);
        operation.set_detail("b", 2.0);
        operation.set_detail("a", 3.0);
        operation.set_detail("c", 4.0);
        operation.set_detail("d", 5.0);
        let details: Vec<_> = operation.details().collect();
        assert_eq!(details, [("a", 3.0), ("b", 2.0), ("c", 4.0)]);
    }
}
