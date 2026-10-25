"""The source generations of the concurrent refresh check: deterministic
edits of the source tables, and the oracle of the graph each generation
builds."""

import math

from harness.generations import (EDGE_QUERY, MARK, NODE_QUERY, Edit, Generations, Source, edit, edit_sql,
                                 first, oracle, setup_sql)


def test_the_first_source_is_a_ring_with_chords_over_every_node():
    source = first(10)
    assert source.nodes == frozenset(range(1, 11))
    assert (10, 1) in source.edges and (1, 2) in source.edges
    assert all(s != d and 1 <= w <= 10 for (s, d), w in source.edges.items())


def test_an_edit_is_a_function_of_its_generation_and_the_source_before_it():
    source = first(100)
    assert edit(source, 7, 100) == edit(source, 7, 100)
    assert edit(source, 7, 100) != edit(source, 8, 100)


def test_an_edit_removes_existing_edges_adds_new_ones_and_never_removes_a_stable_node():
    source = first(100)
    for gen in range(1, 40):
        change = edit(source, gen, 100)
        assert set(change.removed_edges) <= set(source.edges)
        assert not set(change.added_edges) & (set(source.edges) - set(change.removed_edges))
        assert all(node > 50 for node in change.removed_nodes)
        assert change.removed_nodes <= source.nodes and not change.added_nodes & source.nodes
        source = change.apply(source)
        assert set(range(1, 51)) <= source.nodes


def test_generations_are_applied_in_order_and_remembered():
    generations = Generations(100)
    third = generations.source(3)
    expected = first(100)
    for gen in (1, 2, 3):
        expected = edit(expected, gen, 100).apply(expected)
    assert third == expected and generations.source(3) is third


def test_the_oracle_adds_the_marker_and_drops_edges_of_missing_nodes():
    source = Source(frozenset({1, 2, 3}), {(1, 2): 2.0, (2, 3): 5.0, (1, 3): 9.0, (3, 4): 1.0})
    graph = oracle(source, 6)
    assert graph.node_count == 5  # 1, 2, 3, the hub 0 and the marker
    assert graph.edge_count == 4  # the three live edges and 0 → the marker
    assert graph.dropped_edges == 1
    assert graph.out(0) == [MARK + 6]
    assert graph.out(1) == [2, 3]
    assert graph.reachable(1, 1) == {1, 2, 3}
    assert graph.reachable(2, None) == {2, 3}
    assert graph.cost(1, 3) == 7.0 and graph.cost(3, 1) is None
    assert graph.ends(1, 2) == {3}
    assert math.isclose(graph.weight_sum, 17.0)


def test_the_queries_read_the_generation_from_the_state_table():
    assert "public.state" in NODE_QUERY and "public.state" in EDGE_QUERY
    assert str(MARK) in NODE_QUERY and str(MARK) in EDGE_QUERY


def test_setup_loads_the_tables_and_the_generation():
    sql = setup_sql(Source(frozenset({1, 2}), {(1, 2): 3.0}))
    assert "INSERT INTO public.nodes VALUES (1), (2)" in sql
    assert "INSERT INTO public.edges VALUES (1, 2, 3.0)" in sql
    assert "INSERT INTO public.state VALUES (0)" in sql


def test_an_edits_sql_changes_the_tables_and_records_its_generation():
    change = Edit(removed_edges=((1, 2),), added_edges={(2, 1): 4.0}, removed_nodes=frozenset({3}),
                  added_nodes=frozenset({4}))
    sql = edit_sql(change, 9)
    assert "DELETE FROM public.edges WHERE (src, dst) IN ((1, 2))" in sql
    assert "INSERT INTO public.edges VALUES (2, 1, 4.0)" in sql
    assert "DELETE FROM public.nodes WHERE id IN (3)" in sql
    assert "INSERT INTO public.nodes VALUES (4)" in sql
    assert sql.rstrip().endswith("UPDATE public.state SET gen = 9")
