//! What a backend's slot of the activity table holds (DESIGN.md §5.7):
//! who the backend is, what its private cache holds and the shared bytes it
//! maps, the compute threads its call holds, its cache counters and its
//! progress (`frame`), as plain values (the backend's own copy) and as the
//! words its slot stores them in.

use pgrx::pg_sys::Oid;

use super::change_count::Counted;
#[cfg(test)]
use super::frame::Name;
use super::frame::{Frame, Operation, FRAME_WORDS, OPERATION_WORDS};

/// Why the shared cache did not hold a version or vector that was read,
/// which was then read privately (DESIGN.md §5.6, Fallback): a counter
/// each in `warren.backends().refusals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Larger than `warren.shared_cache_memory`.
    TooLarge,
    /// No room beside what is pinned, mapped, loading or draining.
    NoRoom,
    /// No registry slot free.
    NoSlot,
    /// No room for its segment where segments are created (`/dev/shm`).
    StoreFull,
    /// No DSM control slot free.
    NoSegment,
    /// Created by the reading transaction, so not yet visible to others.
    Uncommitted,
    /// The shared cache is off or unavailable to the backend.
    Unavailable,
}

impl Refused {
    /// Every reason, in the order the counters store them.
    pub(crate) const ALL: [Self; REFUSALS] = [
        Self::TooLarge,
        Self::NoRoom,
        Self::NoSlot,
        Self::StoreFull,
        Self::NoSegment,
        Self::Uncommitted,
        Self::Unavailable,
    ];

    /// Its key in `refusals`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::NoRoom => "no_room",
            Self::NoSlot => "no_slot",
            Self::StoreFull => "store_full",
            Self::NoSegment => "no_segment",
            Self::Uncommitted => "uncommitted",
            Self::Unavailable => "unavailable",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The number of refusal reasons.
const REFUSALS: usize = 7;

/// A backend's cache counters: since it started, or, summed, since the
/// table was created (DESIGN.md §5.7). Each counts once per statement and
/// version or vector, as the statement pin is taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// Versions and vectors read: one of each of the four below.
    pub(crate) lookups: u64,
    pub(crate) private_hits: u64,
    /// An attach, or the backend's own mapping.
    pub(crate) shared_hits: u64,
    /// Lookups that found the version loading in another backend.
    pub(crate) waits: u64,
    pub(crate) private_loads: u64,
    pub(crate) shared_loads: u64,
    pub(crate) loaded_bytes: u64,
    pub(crate) load_microseconds: u64,
    pub(crate) private_evictions: u64,
    /// Those this backend made.
    pub(crate) shared_evictions: u64,
    pub(crate) refusals: [u64; REFUSALS],
}

/// The number of words [`Counters`] take.
pub(super) const COUNTER_WORDS: usize = 10 + REFUSALS;

impl Counters {
    /// Counts one refusal for `reason`.
    pub(crate) fn refuse(&mut self, reason: Refused) {
        self.refusals[reason.index()] += 1;
    }

    /// The refusals for `reason`.
    pub(crate) fn refused(&self, reason: Refused) -> u64 {
        self.refusals[reason.index()]
    }

    /// These counters and `other` added up.
    pub(crate) fn plus(&self, other: &Self) -> Self {
        Self::decode(&add(&self.encode(), &other.encode()))
    }

    pub(super) fn encode(&self) -> [u64; COUNTER_WORDS] {
        let mut words = [0; COUNTER_WORDS];
        let scalars = [
            self.lookups,
            self.private_hits,
            self.shared_hits,
            self.waits,
            self.private_loads,
            self.shared_loads,
            self.loaded_bytes,
            self.load_microseconds,
            self.private_evictions,
            self.shared_evictions,
        ];
        words[..10].copy_from_slice(&scalars);
        words[10..].copy_from_slice(&self.refusals);
        words
    }

    pub(super) fn decode(words: &[u64; COUNTER_WORDS]) -> Self {
        let mut refusals = [0; REFUSALS];
        refusals.copy_from_slice(&words[10..]);
        Self {
            lookups: words[0],
            private_hits: words[1],
            shared_hits: words[2],
            waits: words[3],
            private_loads: words[4],
            shared_loads: words[5],
            loaded_bytes: words[6],
            load_microseconds: words[7],
            private_evictions: words[8],
            shared_evictions: words[9],
            refusals,
        }
    }
}

/// Two arrays of words added up, saturating.
fn add<const N: usize>(a: &[u64; N], b: &[u64; N]) -> [u64; N] {
    std::array::from_fn(|i| a[i].saturating_add(b[i]))
}

/// What a backend's private cache holds, and the shared bytes it maps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Memory {
    /// Privately loaded versions, and versions evicted that calls hold.
    pub(crate) versions: u64,
    /// Cached persisted vectors.
    pub(crate) vectors: u64,
    /// Transient vectors, of every role.
    pub(crate) transients: u64,
    /// Cached versions, persisted vectors and transient vectors.
    pub(crate) entries: u64,
    /// Shared versions and vectors the backend maps.
    pub(crate) mapped: u64,
}

/// Who holds a slot: none if its `pid` is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) pid: i32,
    pub(crate) database: Oid,
    pub(crate) role: Oid,
    /// When the backend started, as a `timestamptz`.
    pub(crate) started: i64,
}

impl Holder {
    /// A free slot's.
    pub(crate) const NONE: Self =
        Self { pid: 0, database: Oid::INVALID, role: Oid::INVALID, started: 0 };
}

/// Everything a slot holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Values {
    pub(crate) holder: Holder,
    pub(crate) memory: Memory,
    /// The compute threads the backend's running call holds (DESIGN.md
    /// §14 item 52), 0 between calls.
    pub(crate) compute_threads: u32,
    pub(crate) counters: Counters,
    /// The statement's command, from `warren.report_progress`.
    pub(crate) command: Frame,
    /// The native call running now.
    pub(crate) operation: Operation,
}

impl Values {
    /// A free slot's values.
    pub(crate) const FREE: Self = Self {
        holder: Holder::NONE,
        memory: Memory::ZERO,
        compute_threads: 0,
        counters: Counters::ZERO,
        command: Frame::NONE,
        operation: Operation::NONE,
    };

    pub(super) fn encode(&self) -> [u64; SLOT_WORDS] {
        let Holder { pid, database, role, started } = self.holder;
        let Memory { versions, vectors, transients, entries, mapped } = self.memory;
        let mut words = [0; SLOT_WORDS];
        let head = [
            u64::from(pid as u32),
            u64::from(database.to_u32()),
            u64::from(role.to_u32()),
            started as u64,
            versions,
            vectors,
            transients,
            entries,
            mapped,
            u64::from(self.compute_threads),
        ];
        words[..HEAD_WORDS].copy_from_slice(&head);
        words[HEAD_WORDS..COMMAND_AT].copy_from_slice(&self.counters.encode());
        self.command.encode(&mut words[COMMAND_AT..OPERATION_AT]);
        self.operation.encode(&mut words[OPERATION_AT..]);
        words
    }

    pub(super) fn decode(words: &[u64; SLOT_WORDS]) -> Self {
        let oid = |word: u64| Oid::from(word as u32);
        let mut counters = [0; COUNTER_WORDS];
        counters.copy_from_slice(&words[HEAD_WORDS..COMMAND_AT]);
        Self {
            holder: Holder {
                pid: words[0] as u32 as i32,
                database: oid(words[1]),
                role: oid(words[2]),
                started: words[3] as i64,
            },
            memory: Memory {
                versions: words[4],
                vectors: words[5],
                transients: words[6],
                entries: words[7],
                mapped: words[8],
            },
            compute_threads: words[9] as u32,
            counters: Counters::decode(&counters),
            command: Frame::decode(&words[COMMAND_AT..OPERATION_AT]),
            operation: Operation::decode(&words[OPERATION_AT..]),
        }
    }
}

impl Memory {
    const ZERO: Self = Self { versions: 0, vectors: 0, transients: 0, entries: 0, mapped: 0 };
}

impl Counters {
    const ZERO: Self = Self {
        lookups: 0,
        private_hits: 0,
        shared_hits: 0,
        waits: 0,
        private_loads: 0,
        shared_loads: 0,
        loaded_bytes: 0,
        load_microseconds: 0,
        private_evictions: 0,
        shared_evictions: 0,
        refusals: [0; REFUSALS],
    };
}

/// The words of the holder, the memory and the compute threads.
const HEAD_WORDS: usize = 10;

/// Where the frames start.
const COMMAND_AT: usize = HEAD_WORDS + COUNTER_WORDS;
const OPERATION_AT: usize = COMMAND_AT + FRAME_WORDS;

/// The words a slot stores.
pub(super) const SLOT_WORDS: usize = OPERATION_AT + OPERATION_WORDS;

/// A backend's slot in shared memory.
pub(super) type Slot = Counted<SLOT_WORDS>;

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        let mut counters =
            Counters { lookups: 9, waits: 2, load_microseconds: 77, ..Counters::ZERO };
        counters.refuse(Refused::Unavailable);
        counters.refuse(Refused::TooLarge);
        let command = Frame { name: Name::new("louvain"), start: 9, ..Frame::NONE };
        let mut operation = Operation { frame: command, ..Operation::NONE };
        operation.frame.name = Name::new("local_moving");
        operation.enter("moving", Some(100));
        operation.set_detail("moved", 17.0);
        Values {
            holder: Holder { pid: 4321, database: Oid::from(5), role: Oid::from(10), started: -3 },
            memory: Memory { versions: 1, vectors: 2, transients: 3, entries: 4, mapped: 5 },
            compute_threads: 6,
            counters,
            command,
            operation,
        }
    }

    #[test]
    fn values_round_trip_through_their_words() {
        let values = values();
        assert_eq!(Values::decode(&values.encode()), values);
        assert_eq!(Values::decode(&[0; SLOT_WORDS]), Values::FREE, "a zeroed slot is free");
    }

    #[test]
    fn refusals_are_counted_by_reason() {
        let counters = values().counters;
        assert_eq!(counters.refused(Refused::Unavailable), 1);
        assert_eq!(counters.refused(Refused::TooLarge), 1);
        assert_eq!(counters.refused(Refused::NoRoom), 0);
        let names: Vec<_> = Refused::ALL.iter().map(|reason| reason.name()).collect();
        assert_eq!(
            names,
            [
                "too_large",
                "no_room",
                "no_slot",
                "store_full",
                "no_segment",
                "uncommitted",
                "unavailable"
            ]
        );
        assert!(Refused::ALL.iter().enumerate().all(|(i, reason)| reason.index() == i));
    }

    #[test]
    fn counters_add_up() {
        let a = values().counters;
        let sum = a.plus(&a);
        assert_eq!((sum.lookups, sum.waits, sum.refused(Refused::TooLarge)), (18, 4, 2));
        assert_eq!(Counters::decode(&a.encode()), a);
    }

    #[test]
    fn a_slot_is_about_half_a_kilobyte() {
        assert!(std::mem::size_of::<Slot>() <= 512, "{}", std::mem::size_of::<Slot>());
    }

    #[test]
    fn slots_share_no_cache_line() {
        // Every write stores all of a slot's words: a line shared with the
        // next slot would bounce between two backends' cores.
        assert_eq!(std::mem::align_of::<Slot>() % 64, 0);
        assert_eq!(std::mem::size_of::<Slot>() % 64, 0);
    }
}
