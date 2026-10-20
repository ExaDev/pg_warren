//! The relation dictionary: relation names to relation indexes, append-only
//! across versions of a graph (DESIGN.md §5.3, `rel_dict`).

use std::collections::HashMap;
use std::mem::size_of;

use super::{BuildCharge, BuildMemory, GraphError};

/// The position of a relation in its relation dictionary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationIndex(u32);

impl RelationIndex {
    /// The index as a position into per-relation arrays.
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The index at `position` in per-relation arrays.
    pub(crate) fn at(position: usize) -> Result<Self, GraphError> {
        u32::try_from(position).map(Self).map_err(|_| GraphError::TooManyRelations)
    }
}

/// Which relations a traversal or compute kernel reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationSelection<'a> {
    /// Every relation.
    All,
    /// Only these relations; repeats are ignored.
    Only(&'a [RelationIndex]),
}

/// Relation names in index order. Names are only ever appended, so a
/// relation keeps its index in every later version of the graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelationDictionary {
    names: Vec<String>,
    indexes: HashMap<String, RelationIndex>,
}

impl RelationDictionary {
    /// An empty dictionary.
    pub fn new() -> Self {
        Self::default()
    }

    /// The index of `name`, appending it if it is new.
    pub fn intern(&mut self, name: &str) -> Result<RelationIndex, GraphError> {
        self.intern_within(name, &mut BuildCharge::none(&BuildMemory::unlimited()))
    }

    /// [`Self::intern`], charging a new name to `charge`: its two copies
    /// before they are made, and the tables' growth, which is reserved
    /// fallibly first. A build's dictionary grows with the distinct
    /// relation names its edge rows hold, which a mistaken relation column
    /// can make as many as the rows (DESIGN.md §5.4).
    pub fn intern_within(
        &mut self,
        name: &str,
        charge: &mut BuildCharge,
    ) -> Result<RelationIndex, GraphError> {
        if let Some(&index) = self.indexes.get(name) {
            return Ok(index);
        }
        let index = RelationIndex::at(self.names.len())?;
        let tables = self.table_bytes();
        let out_of_memory = GraphError::OutOfMemory { elements: self.names.len() + 1 };
        self.names.try_reserve(1).map_err(|_| out_of_memory.clone())?;
        self.indexes.try_reserve(1).map_err(|_| out_of_memory)?;
        charge.grow((2 * name.len() as u64).saturating_add(self.table_bytes() - tables))?;
        self.names.push(name.to_owned());
        self.indexes.insert(name.to_owned(), index);
        Ok(index)
    }

    /// The index of `name`, if it is in the dictionary.
    pub fn get(&self, name: &str) -> Option<RelationIndex> {
        self.indexes.get(name).copied()
    }

    /// The name of the relation at `index`.
    pub fn name(&self, index: RelationIndex) -> Option<&str> {
        self.names.get(index.index()).map(String::as_str)
    }

    /// All names, in index order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The number of relations.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the dictionary has no relations.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The heap bytes it holds, approximately: the names (stored twice,
    /// in order and as index keys) and the two tables.
    pub fn bytes(&self) -> u64 {
        let names: u64 = self.names.iter().map(|name| name.capacity() as u64).sum();
        2 * names + self.table_bytes()
    }

    /// The bytes of the two tables, without the names: the ordered names'
    /// slots, and the index's buckets (its capacity is 7/8 of them, a
    /// power of two) with a control byte each.
    fn table_bytes(&self) -> u64 {
        let capacity = self.indexes.capacity();
        let buckets = match capacity {
            0 => 0,
            1..8 => (capacity + 1).next_power_of_two(),
            _ => (capacity * 8 / 7).next_power_of_two(),
        };
        let entry = size_of::<(String, RelationIndex)>() + 1;
        (self.names.capacity() * size_of::<String>() + buckets * entry) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_assigns_indexes_in_first_seen_order() {
        let mut relations = RelationDictionary::new();
        let cites = relations.intern("cites").unwrap();
        let authored = relations.intern("authored").unwrap();
        assert_eq!(relations.intern("cites"), Ok(cites));
        assert_eq!(cites.index(), 0);
        assert_eq!(authored.index(), 1);
        assert_eq!(relations.len(), 2);
        assert_eq!(relations.get("authored"), Some(authored));
        assert_eq!(relations.get("funds"), None);
        assert_eq!(relations.name(authored), Some("authored"));
        assert_eq!(relations.names(), ["cites", "authored"]);
    }

    #[test]
    fn a_later_build_appends_to_the_previous_dictionary() {
        let mut previous = RelationDictionary::new();
        previous.intern("a").unwrap();
        previous.intern("b").unwrap();
        let mut next = previous.clone();
        let c = next.intern("c").unwrap();
        assert_eq!(next.get("a"), previous.get("a"));
        assert_eq!(c.index(), 2);
    }

    #[test]
    fn interning_charges_each_new_name_and_the_tables_growth() {
        let memory = BuildMemory::new(10_000);
        let mut charge = BuildCharge::none(&memory);
        let mut relations = RelationDictionary::new();
        relations.intern_within("cites", &mut charge).unwrap();
        relations.intern_within("cites", &mut charge).unwrap();
        relations.intern_within("authored", &mut charge).unwrap();
        assert_eq!(charge.bytes(), relations.bytes());
        let mut tight = BuildCharge::none(&BuildMemory::new(charge.bytes()));
        let mut copy = RelationDictionary::new();
        copy.intern_within("cites", &mut tight).unwrap();
        assert!(matches!(
            copy.intern_within(&"x".repeat(5_000), &mut tight),
            Err(GraphError::BuildMemoryLimit { .. })
        ));
        assert_eq!(copy.len(), 1, "a refused name is not added");
        assert_eq!(relations.encoded_bytes(), relations.encode().unwrap().len() as u64);
    }

    #[test]
    fn a_dictionary_holds_at_least_its_names_twice() {
        let mut relations = RelationDictionary::new();
        assert_eq!(relations.bytes(), 0);
        relations.intern("cites").unwrap();
        relations.intern("authored").unwrap();
        assert!(relations.bytes() >= 2 * ("cites".len() + "authored".len()) as u64);
    }
}
