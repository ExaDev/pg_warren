//! The relation dictionary: relation names to relation indexes, append-only
//! across versions of a graph (DESIGN.md §5.3, `rel_dict`).

use std::collections::HashMap;

use super::GraphError;

/// The position of a relation in its relation dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationIndex(u32);

impl RelationIndex {
    /// The index as a position into per-relation arrays.
    pub fn index(self) -> usize {
        self.0 as usize
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
        if let Some(&index) = self.indexes.get(name) {
            return Ok(index);
        }
        let index = u32::try_from(self.names.len())
            .map(RelationIndex)
            .map_err(|_| GraphError::TooManyRelations)?;
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
}
