-- Neighbourhood similarity against NetworkX (DESIGN.md §6.7, §12) on a
-- directed random graph read as undirected and simple, widened with
-- self-loops and with edges in both directions and in a second relation:
-- every ordered pair's Jaccard, Adamic–Adar and common neighbours (within
-- 1e-12) on a graph with reverse CSRs (read node by node) and one without
-- (the symmetric adjacency built once); each node's top 5 by Jaccard and by
-- common neighbours is NetworkX's ranking of the non-edges sharing a
-- neighbour, ties by id; Adamic–Adar's top 5 score as NetworkX does.
-- fixture: random --nodes 200 --edges 900 --directed --seed 17
-- oracle: similarity --directed --edges-sql 'SELECT src, dst FROM edges UNION ALL SELECT dst, src FROM edges WHERE (src + dst) % 3 = 0 UNION ALL SELECT id, id FROM nodes WHERE id % 11 = 0'
CREATE TABLE s_edges AS
SELECT src, dst, 'a'::text AS rel FROM edges
UNION ALL SELECT dst, src, 'b' FROM edges WHERE (src + dst) % 3 = 0
UNION ALL SELECT id, id, 'a' FROM nodes WHERE id % 11 = 0;
SELECT warren.create_graph('s', 'SELECT id FROM nodes', 'SELECT src, dst, rel FROM s_edges', has_relations := true);
SELECT warren.create_graph('n', 'SELECT id FROM nodes', 'SELECT src, dst, rel FROM s_edges', has_relations := true,
                           build_reverse := false);
SELECT warren.refresh('s'), warren.refresh('n');
CREATE TABLE adjacent AS SELECT src AS source, dst AS target FROM s_edges WHERE src <> dst
                         UNION SELECT dst, src FROM s_edges WHERE src <> dst;
SELECT assert.equal((SELECT count(*) FROM oracle_similarity), 200 * 199::bigint, 'every ordered pair');
SELECT assert.that((SELECT count(*) FROM oracle_similarity WHERE common > 1) > 1000, 'many pairs share neighbours');

CREATE FUNCTION scored(graph text, measure text) RETURNS TABLE (source bigint, target bigint, score float8)
LANGUAGE sql AS $$
    SELECT * FROM warren.similarity(graph, 'SELECT source, target FROM oracle_similarity', NULL::bigint, measure)
$$;
SELECT assert.equal((SELECT count(*) FROM scored(g, m) s JOIN oracle_similarity o USING (source, target)
                     WHERE abs(s.score - CASE m WHEN 'jaccard' THEN o.jaccard WHEN 'adamic_adar' THEN o.adamic_adar
                                                ELSE o.common END) <= 1e-12),
                    200 * 199::bigint, format('%s on %s agrees with NetworkX', m, g))
FROM (VALUES ('s'), ('n')) g(g), (VALUES ('jaccard'), ('adamic_adar'), ('common_neighbours')) m(m);

-- NetworkX's ranking: non-edges sharing a neighbour, best first, ties by id
-- (dense ids follow ids on a first build).
CREATE FUNCTION ranked(measure text) RETURNS TABLE (source bigint, target bigint, score float8)
LANGUAGE sql AS $$
    SELECT source, target, score FROM (
        SELECT o.source, o.target, CASE measure WHEN 'jaccard' THEN o.jaccard ELSE o.common END AS score,
               row_number() OVER (PARTITION BY o.source ORDER BY CASE measure WHEN 'jaccard' THEN o.jaccard
                                                                       ELSE o.common END DESC, o.target) AS r
        FROM oracle_similarity o
        WHERE o.common > 0 AND NOT EXISTS (SELECT 1 FROM adjacent a WHERE (a.source, a.target) = (o.source, o.target))
    ) t WHERE r <= 5
$$;
SELECT assert.equal((SELECT count(*) FROM ((SELECT * FROM ranked(m) EXCEPT
                                            SELECT * FROM warren.similar_pairs(g, NULL::bigint, k := 5, measure := m))
                                           UNION ALL (SELECT * FROM warren.similar_pairs(g, NULL::bigint, k := 5, measure := m)
                                                      EXCEPT SELECT * FROM ranked(m))) d),
                    0::bigint, format('similar_pairs by %s on %s is NetworkX''s ranking', m, g))
FROM (VALUES ('s'), ('n')) g(g), (VALUES ('jaccard'), ('common_neighbours')) m(m);
SELECT assert.equal((SELECT count(*) FROM warren.similar_pairs('s', NULL::bigint, k := 5, measure := 'adamic_adar') s
                     JOIN oracle_similarity o USING (source, target)
                     WHERE abs(s.score - o.adamic_adar) <= 1e-12 AND o.common > 0),
                    (SELECT count(*) FROM ranked('common_neighbours')),
                    'similar_pairs by adamic_adar ranks as many pairs and scores them as NetworkX does');
SELECT assert.equal((SELECT count(*) FROM generate_series(1, 200, 37) u(u),
                         LATERAL (SELECT * FROM warren.most_similar('s', u::bigint, k := 5)
                                  EXCEPT SELECT target, score FROM ranked('jaccard') r WHERE r.source = u) d),
                    0::bigint, 'most_similar is one node''s similar_pairs');
