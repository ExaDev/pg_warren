//! A graph built in locality order is the graph built in id order under the
//! permutation (DESIGN.md §14 item 22): the same edges between the same
//! external ids, with the same weights and deduplicated edges, in rows
//! sorted as the CSR invariants require; and traversal and compute results,
//! read by external id, are identical.

// In a `cfg(test)` module, so the lints that allow `unwrap` in tests apply
// to its helpers too.
#[cfg(test)]
mod locality {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use warren_core::compute::{
        spmv, AnyVector, ComputeMemory, Product, Semiring, Vector, Workers,
    };
    use warren_core::graph::{
        AnyCsr, BuildMemory, CsrLayout, DenseId, Direction, EdgeIntake, GraphCsr, IdMap, NodeOrder,
        Permutation, RelationDictionary, RelationSelection,
    };
    use warren_core::interrupts::Uninterrupted;
    use warren_core::traversal::{bfs, components, Adjacency, Limits};

    /// An edge row: (relation, source, target, weight).
    type Row = (u8, i64, i64, u8);

    /// A graph built from external ids in one node order.
    struct Built {
        id_map: IdMap<i64>,
        graph: GraphCsr,
    }

    /// What a build is asked for besides its rows.
    #[derive(Debug, Clone, Copy)]
    struct Shape {
        directed: bool,
        weighted: bool,
        layout: CsrLayout,
    }

    fn build(nodes: &[i64], rows: &[Row], shape: Shape, order: NodeOrder) -> Built {
        let mut id_map = IdMap::new().rebuild(nodes.to_vec()).unwrap();
        let mut relations = RelationDictionary::new();
        let mut intake = match shape.directed {
            true => EdgeIntake::directed(&id_map, shape.weighted),
            false => EdgeIntake::undirected(&id_map, shape.weighted),
        };
        for &(relation, source, target, weight) in rows {
            let relation = relations.intern(&format!("r{relation}")).unwrap();
            intake.push(relation, Some(&source), Some(&target), f64::from(weight)).unwrap();
        }
        let (mut edges, _) = intake.finish();
        let id_space = id_map.id_space();
        if order == NodeOrder::Locality {
            let permutation =
                Permutation::locality(&edges, id_space, &BuildMemory::unlimited()).unwrap();
            edges.renumber(&permutation).unwrap();
            id_map.renumber(&permutation).unwrap();
        }
        let graph = GraphCsr::build_as(edges, relations.len(), id_space, shape.layout).unwrap();
        Built { id_map, graph }
    }

    impl Built {
        fn external(&self, dense: DenseId) -> i64 {
            self.id_map.external_id(dense).unwrap()
        }

        fn dense(&self, external: i64) -> DenseId {
            self.id_map.dense_id(&external).unwrap()
        }

        /// A CSR's edges in external ids, with their weights' bits, after
        /// checking that each row is strictly ascending.
        fn edges(&self, csr: &AnyCsr) -> BTreeSet<(i64, i64, u64)> {
            let mut edges = BTreeSet::new();
            for source in 0..csr.id_space() {
                let row: Vec<_> = csr.neighbours(source).collect();
                assert!(row.windows(2).all(|pair| pair[0] < pair[1]), "row {source}: {row:?}");
                let weights = csr.neighbour_weights(source);
                for (i, &target) in row.iter().enumerate() {
                    let weight = weights.map_or(1.0, |weights| weights[i]);
                    edges.insert((self.external(source), self.external(target), weight.to_bits()));
                }
            }
            edges
        }

        /// Every stored CSR's edges: forward and reverse, per relation and
        /// merged.
        fn all_edges(&self) -> Vec<BTreeSet<(i64, i64, u64)>> {
            let csrs = self.graph.relations().iter().chain(self.graph.merged());
            csrs.flat_map(|csr| [Some(csr.forward()), csr.reverse()])
                .flatten()
                .map(|csr| self.edges(csr))
                .collect()
        }

        /// Each node's BFS depth from `start`, by external id.
        fn depths(&self, start: i64, direction: Direction) -> BTreeMap<i64, u32> {
            let adjacency = Adjacency::new(&self.graph, direction, RelationSelection::All).unwrap();
            let starts = [self.dense(start)];
            let visits = bfs(&adjacency, &starts, None, Limits::default(), &mut Uninterrupted)
                .unwrap()
                .result;
            visits.iter().map(|visit| (self.external(visit.node), visit.depth)).collect()
        }

        /// The weakly connected components, as sets of external ids.
        fn components(&self) -> BTreeSet<BTreeSet<i64>> {
            let labels = components(&self.graph, &mut Uninterrupted).unwrap().result;
            let mut by_label: BTreeMap<DenseId, BTreeSet<i64>> = BTreeMap::new();
            for (dense, &label) in labels.iter().enumerate() {
                by_label.entry(label).or_default().insert(self.external(dense as DenseId));
            }
            by_label.into_values().collect()
        }

        /// `y = A x` with `x` the external id of each node, by external id.
        fn product(&self, transpose: bool) -> BTreeMap<i64, f64> {
            let memory = ComputeMemory::new(u64::MAX);
            let id_space = self.id_map.id_space();
            let values = (0..id_space).map(|dense| self.external(dense) as f64).collect();
            let x = AnyVector::from(Vector::from_values(&memory, id_space, 1, values).unwrap());
            let mut y = AnyVector::from(Vector::filled(&memory, id_space, 1, 0.0f64).unwrap());
            let product = Product { transpose, ..Product::new(Semiring::PlusTimes) };
            let workers = Workers::new(1).unwrap();
            spmv(&workers, &memory, &mut Uninterrupted, &self.graph, &product, &x, &mut y).unwrap();
            let AnyVector::F64(y) = y else { panic!("an f64 product") };
            (0..id_space).map(|dense| (self.external(dense), y.values()[dense as usize])).collect()
        }
    }

    fn graphs() -> impl Strategy<Value = (Vec<i64>, Vec<Row>)> {
        let nodes = prop::collection::vec(0i64..40, 1..30);
        let rows = prop::collection::vec((0u8..3, 0i64..40, 0i64..40, 1u8..5), 0..120);
        (nodes, rows)
    }

    proptest! {
        #[test]
        fn a_locality_build_is_the_id_build_under_the_permutation(
            (nodes, rows) in graphs(),
            directed in any::<bool>(),
            weighted in any::<bool>(),
            build_reverse in any::<bool>(),
            store_merged in any::<bool>(),
        ) {
            let shape = Shape { directed, weighted, layout: CsrLayout { build_reverse, store_merged } };
            let by_id = build(&nodes, &rows, shape, NodeOrder::Id);
            let local = build(&nodes, &rows, shape, NodeOrder::Locality);

            prop_assert_eq!(local.graph.layout(), by_id.graph.layout());
            prop_assert_eq!(local.graph.deduplicated_edges(), by_id.graph.deduplicated_edges());
            prop_assert_eq!(local.all_edges(), by_id.all_edges());
            let mut externals: Vec<i64> = (0..local.id_map.id_space())
                .map(|dense| local.external(dense))
                .collect();
            externals.sort_unstable();
            prop_assert!(externals.iter().all(|&e| by_id.id_map.dense_id(&e).is_some()));
            prop_assert_eq!(externals.len() as u64, by_id.id_map.id_space());
        }

        #[test]
        fn results_read_by_external_id_do_not_depend_on_the_node_order(
            (nodes, rows) in graphs(),
            directed in any::<bool>(),
            store_merged in any::<bool>(),
        ) {
            let layout = CsrLayout { build_reverse: true, store_merged };
            let shape = Shape { directed, weighted: true, layout };
            let by_id = build(&nodes, &rows, shape, NodeOrder::Id);
            let local = build(&nodes, &rows, shape, NodeOrder::Locality);

            for direction in [Direction::Out, Direction::In, Direction::Both] {
                prop_assert_eq!(local.depths(nodes[0], direction), by_id.depths(nodes[0], direction));
            }
            prop_assert_eq!(local.components(), by_id.components());
            // Integer weights and values: every sum is exact in any order.
            for transpose in [false, true] {
                prop_assert_eq!(local.product(transpose), by_id.product(transpose));
            }
        }
    }
}
