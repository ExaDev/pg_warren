"""bench/eigen.py: the ARPACK baseline for `warren.eigen`."""

import numpy as np

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
