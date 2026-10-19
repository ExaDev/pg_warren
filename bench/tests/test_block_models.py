"""Stochastic block models (A.2, A.5) and evolving graphs (A.2 ingest variants)."""

import numpy as np
import pytest

from fixtures import evolving, sbm

SMALL = dict(communities=5, community_size=60, p_in=0.2, p_out=0.01)


def columns(table, *names):
    return [table.column(n) for n in names]


def cosine(a, b):
    return float(a @ b / np.linalg.norm(a) / np.linalg.norm(b))


class TestBlockModel:
    def test_nodes_are_numbered_in_contiguous_communities(self):
        nodes = sbm.generate(**SMALL, seed=1).table("nodes")
        ids, community = columns(nodes, "id", "community")
        assert ids.tolist() == list(range(1, 301))
        assert (community == (ids - 1) // 60).all()

    def test_edges_are_distinct_and_split_into_within_and_between_as_the_truth_says(self):
        fixture = sbm.generate(**SMALL, seed=1)
        src, dst = columns(fixture.table("edges"), "src", "dst")
        assert len(set(zip(src.tolist(), dst.tolist()))) == len(src) == fixture.truth["edges"]
        assert (src < dst).all()
        within = int(((src - 1) // 60 == (dst - 1) // 60).sum())
        assert within == fixture.truth["edges_within"]
        assert len(src) - within == fixture.truth["edges_between"]

    def test_edge_densities_follow_p_in_and_p_out(self):
        truth = sbm.generate(communities=4, community_size=200, p_in=0.05, p_out=0.002, seed=2).truth
        within_pairs = 4 * 200 * 199 / 2
        between_pairs = 6 * 200 * 200
        assert truth["edges_within"] / within_pairs == pytest.approx(0.05, rel=0.1)
        assert truth["edges_between"] / between_pairs == pytest.approx(0.002, rel=0.2)

    def test_embeddings_cluster_around_their_community_centroid(self):
        fixture = sbm.generate(**SMALL, seed=1, embedding_dim=16)
        ids, embedding = columns(fixture.table("embeddings"), "id", "embedding")
        assert embedding.shape == (300, 16) and embedding.dtype == np.float32
        same = np.mean([cosine(embedding[i], embedding[i + 1]) for i in range(0, 59)])
        other = np.mean([cosine(embedding[i], embedding[i + 60]) for i in range(0, 59)])
        assert same > 0.6 and abs(other) < 0.3
        centroids = fixture.table("truth_centroids")
        assert centroids.column("community").tolist() == list(range(5))

    def test_without_dimensions_there_are_no_embeddings(self):
        with pytest.raises(KeyError):
            sbm.generate(**SMALL, seed=1).table("embeddings")

    def test_relations_label_every_edge(self):
        edges = sbm.generate(**SMALL, seed=1, relations=("related_to", "part_of")).table("edges")
        assert set(edges.column("rel").tolist()) == {"related_to", "part_of"}

    def test_is_deterministic_in_its_seed(self):
        def edges(seed):
            return sbm.generate(**SMALL, seed=seed).table("edges").column("dst").tolist()
        assert edges(4) == edges(4)
        assert edges(4) != edges(5)


def ingest(fixture, variant):
    nodes, edges = fixture.table("ingest_nodes"), fixture.table("ingest_edges")
    n = nodes.column("variant") == variant
    e = edges.column("variant") == variant
    return (nodes.column("id")[n], nodes.column("community")[n],
            edges.column("src")[e], edges.column("dst")[e])


class TestEvolving:
    PARAMS = dict(SMALL, new_entities=40, new_edges=200, seed=3)

    def test_the_base_graph_is_the_block_model_with_the_same_seed(self):
        base = sbm.generate(**SMALL, seed=3).table("edges").column("src").tolist()
        assert evolving.generate(**self.PARAMS).table("edges").column("src").tolist() == base

    @pytest.mark.parametrize("variant", evolving.VARIANTS)
    def test_each_variant_adds_the_requested_entities_and_distinct_edges(self, variant):
        fixture = evolving.generate(**self.PARAMS)
        ids, _, src, dst = ingest(fixture, variant)
        assert ids.tolist() == list(range(301, 341))
        assert len(set(zip(src.tolist(), dst.tolist()))) == len(src) == 200
        assert ((src > 300) | (dst > 300)).all()
        assert fixture.truth["variants"][variant]["nodes"] == 40
        assert fixture.truth["variants"][variant]["edges"] == 200

    def test_volume_entities_join_existing_communities_mostly_inside_them(self):
        fixture = evolving.generate(**self.PARAMS)
        ids, community, src, dst = ingest(fixture, "volume")
        assert set(community.tolist()) <= set(range(5))
        of = dict(zip(ids.tolist(), community.tolist())) | {i: (i - 1) // 60 for i in range(1, 301)}
        inside = np.mean([of[a] == of[b] for a, b in zip(src.tolist(), dst.tolist())])
        assert 0.6 < inside < 0.95

    def test_bridge_entities_link_only_to_the_two_bridged_communities(self):
        fixture = evolving.generate(**self.PARAMS)
        a, b = fixture.truth["variants"]["bridge"]["communities"]
        _, community, src, dst = ingest(fixture, "bridge")
        old = np.where(src <= 300, src, dst)
        assert set(((old - 1) // 60).tolist()) == {a, b}
        assert set(community.tolist()) == {a, b}

    def test_novel_entities_form_a_community_of_their_own(self):
        fixture = evolving.generate(**self.PARAMS)
        _, community, src, dst = ingest(fixture, "novel")
        assert set(community.tolist()) == {5} == {fixture.truth["variants"]["novel"]["community"]}
        assert np.mean((src > 300) & (dst > 300)) > 0.9

    def test_ingest_embeddings_follow_the_variant(self):
        fixture = evolving.generate(**self.PARAMS, embedding_dim=8)
        table = fixture.table("ingest_embeddings")
        assert len(table) == 3 * 40 and table.column("embedding").shape == (120, 8)

    def test_a_variant_does_not_depend_on_which_others_are_generated(self):
        alone = evolving.generate(**self.PARAMS, variants=("bridge",))
        assert ingest(alone, "bridge")[2].tolist() == ingest(evolving.generate(**self.PARAMS), "bridge")[2].tolist()
        assert set(alone.table("ingest_nodes").column("variant").tolist()) == {"bridge"}
