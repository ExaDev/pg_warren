"""Random (Erdős–Rényi), power-law (preferential attachment) and grid graphs."""

import numpy as np
import pytest

from fixtures import grid, power_law, random_graph
from fixtures.edge_sampling import distinct_pairs


def edge_set(fixture):
    edges = fixture.table("edges")
    return list(zip(edges.column("src").tolist(), edges.column("dst").tolist()))


def degrees(fixture):
    edges = fixture.table("edges")
    ends = np.concatenate([edges.column("src"), edges.column("dst")])
    return np.bincount(ends)


class TestDistinctPairs:
    def test_returns_exactly_count_distinct_canonical_pairs_without_self_loops(self):
        rng = np.random.default_rng(1)
        src, dst = distinct_pairs(rng, 400, lambda k: (rng.integers(0, 30, k), rng.integers(0, 30, k)),
                                  directed=False, limit=30 * 29 // 2)
        pairs = list(zip(src.tolist(), dst.tolist()))
        assert len(pairs) == len(set(pairs)) == 400
        assert all(a < b for a, b in pairs)

    def test_directed_pairs_keep_their_orientation(self):
        rng = np.random.default_rng(1)
        src, dst = distinct_pairs(rng, 800, lambda k: (rng.integers(0, 30, k), rng.integers(0, 30, k)),
                                  directed=True, limit=30 * 29)
        assert (src > dst).any() and (src < dst).any()
        assert len(set(zip(src.tolist(), dst.tolist()))) == 800

    def test_refuses_more_pairs_than_exist(self):
        rng = np.random.default_rng(1)
        with pytest.raises(ValueError, match="only 3"):
            distinct_pairs(rng, 4, lambda k: (rng.integers(0, 3, k), rng.integers(0, 3, k)),
                           directed=False, limit=3)


class TestRandomGraph:
    def test_has_the_requested_nodes_and_distinct_edges(self):
        fixture = random_graph.generate(nodes=500, edges=2000, seed=3)
        assert fixture.table("nodes").column("id").tolist() == list(range(1, 501))
        pairs = edge_set(fixture)
        assert len(set(pairs)) == 2000
        assert all(1 <= a < b <= 500 for a, b in pairs)
        assert fixture.truth == {"generator": "random", "nodes": 500, "edges": 2000, "directed": False, "seed": 3}

    def test_directed_graphs_have_both_orientations(self):
        pairs = edge_set(random_graph.generate(nodes=100, edges=500, seed=3, directed=True))
        assert any(a > b for a, b in pairs) and all(a != b for a, b in pairs)

    def test_is_deterministic_in_its_seed(self):
        assert edge_set(random_graph.generate(200, 600, seed=5)) == edge_set(random_graph.generate(200, 600, seed=5))
        assert edge_set(random_graph.generate(200, 600, seed=5)) != edge_set(random_graph.generate(200, 600, seed=6))


class TestPowerLaw:
    def test_every_new_node_attaches_to_m_distinct_earlier_nodes(self):
        fixture = power_law.generate(nodes=2000, attachments=3, seed=1)
        pairs = edge_set(fixture)
        assert len(pairs) == len(set(pairs)) == 3 * (2000 - 3)
        assert all(a > b for a, b in pairs)  # src is the newer node
        assert fixture.truth["edges"] == len(pairs)

    def test_degree_distribution_is_heavy_tailed(self):
        deg = degrees(power_law.generate(nodes=5000, attachments=2, seed=1))[1:]
        assert deg.max() > 10 * deg.mean()

    def test_is_deterministic_in_its_seed(self):
        assert edge_set(power_law.generate(300, 2, seed=9)) == edge_set(power_law.generate(300, 2, seed=9))
        assert edge_set(power_law.generate(300, 2, seed=9)) != edge_set(power_law.generate(300, 2, seed=8))


class TestGrid:
    def test_links_each_cell_to_its_right_and_lower_neighbour(self):
        fixture = grid.generate(rows=3, cols=4)
        assert len(fixture.table("nodes")) == 12
        pairs = edge_set(fixture)
        assert len(pairs) == 3 * 3 + 2 * 4
        assert (1, 2) in pairs and (1, 5) in pairs and (4, 5) not in pairs
        assert set(degrees(fixture)[1:].tolist()) == {2, 3, 4}
