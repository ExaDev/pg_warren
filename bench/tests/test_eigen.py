"""bench/eigen.py: the ARPACK baseline for `warren.eigen`."""

import numpy as np
import pytest

import eigen


def path_edges(n):
    """The undirected path 1 – 2 – … – n, plus isolated node n + 1."""
    return np.arange(1, n), np.arange(2, n + 1)


def test_operators_are_the_warren_matrices_over_active_nodes():
    src, dst = path_edges(4)
    adjacency = eigen.operator_matrix(src, dst, 5, "adjacency").toarray()
    assert adjacency.shape == (4, 4)  # node 5 has no edge: inactive
    assert np.array_equal(adjacency, adjacency.T) and adjacency.sum() == 6
    laplacian = eigen.operator_matrix(src, dst, 5, "normalized_laplacian").toarray()
    assert np.allclose(np.diag(laplacian), 1.0)
    assert np.isclose(laplacian[0, 1], -1 / np.sqrt(2))


def test_arpack_counts_matvecs_and_matches_a_dense_solver():
    src, dst = path_edges(60)
    matrix = eigen.operator_matrix(src, dst, 60, "normalized_laplacian")
    run = eigen.arpack(matrix, k=3, which="smallest", tol=1e-10)
    assert run["matvecs"] > 0 and run["seconds"] >= 0
    dense = np.linalg.eigvalsh(matrix.toarray())[:3]
    assert np.allclose(run["eigenvalues"], dense, atol=1e-8)
    largest = eigen.arpack(matrix, k=2, which="largest", tol=1e-10)["eigenvalues"]
    assert np.allclose(largest, np.linalg.eigvalsh(matrix.toarray())[::-1][:2], atol=1e-8)


def test_random_and_block_model_graphs_have_average_degree_twenty():
    src, dst = eigen.graph_edges("random", 2000, seed=1)
    assert len(src) == 10 * 2000 and (src != dst).all()
    src, dst = eigen.graph_edges("sbm", 4000, seed=1, communities=8)
    assert 2 * len(src) / 4000 == pytest.approx(20, rel=0.05)
    within = ((src - 1) // 500 == (dst - 1) // 500).mean()
    assert within == pytest.approx(0.8, abs=0.03)


def test_a_block_model_needs_equal_communities():
    with pytest.raises(ValueError, match="divide"):
        eigen.graph_edges("sbm", 1001, seed=1, communities=8)


def test_graph_names_say_the_generator_and_its_scale():
    assert eigen.graph_name("random", 100_000, 1, 40) == "random-100000-1000000-seed1"
    assert eigen.graph_name("sbm", 100_000, 1, 40) == "sbm40-100000-1000000-seed1"


def test_gaps_measure_the_wanted_end_against_the_first_unwanted_value():
    # smallest: 0, then a cluster 0.5, 0.6, 0.7 (k = 4), then 1.0 next.
    gaps = eigen.gaps([0.0, 0.5, 0.6, 0.7, 1.0, 1.1], k=4, which="smallest", spread=2.0)
    assert gaps == pytest.approx({"next_gap": 0.3, "mean_spacing": 0.1, "spread": 2.0, "relative_next_gap": 0.15})
    # largest compares magnitudes: |−9| after 21, then 8.5 next.
    gaps = eigen.gaps([21.0, -9.0, 8.5], k=2, which="largest", spread=30.0)
    assert gaps["next_gap"] == pytest.approx(0.5)
    assert gaps["mean_spacing"] is None  # one wanted value after the first


def test_arpack_spread_is_the_width_of_the_spectrum():
    src, dst = path_edges(60)
    matrix = eigen.operator_matrix(src, dst, 60, "normalized_laplacian")
    dense = np.linalg.eigvalsh(matrix.toarray())
    assert eigen.spread(matrix) == pytest.approx(dense[-1] - dense[0], abs=1e-8)


def test_the_warren_command_passes_max_iter_only_when_given():
    command = eigen.warren_command("e.csv", 10, "adjacency", "largest", 2, 1e-6, "1", "f32", None)
    assert command[-8:] == ["e.csv", "10", "adjacency", "largest", "2", "1e-06", "1", "f32"]
    command = eigen.warren_command("e.csv", 10, "adjacency", "largest", 2, 1e-6, "1", "f32", 500)
    assert command[-1] == "500"
