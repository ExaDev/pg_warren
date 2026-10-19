-- Appendix A.2 / §8: corpus ingest gated by structural change, up to diff,
-- for each ingest variant on a small block model. diff measures volume only,
-- so it reports the same kind of delta for all three (documented in A.2).
-- Spectral distance (eigen, distance) joins this test in M6.
-- Dense ids of pre-existing entities are checked to be identical across the
-- base and staged versions.
-- fixture: evolving --communities 6 --community-size 50 --p-in 0.2 --p-out 0.005 --new-entities 30 --new-edges 150 --embedding-dim 8 --seed 2
SELECT warren.create_graph('kg', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('kg') AS base \gset
-- Stable dense ids (A.2), until warren.id_map (M4): in a graph without edges
-- every node is its own component, and warren.components labels it with its
-- own dense id. kg_ids reads the same nodes table.
SELECT warren.create_graph('kg_ids', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges WHERE false', directed := false);
SELECT warren.refresh('kg_ids') AS ids_base \gset
SELECT assert.equal((SELECT count(DISTINCT component_id) FROM warren.components('kg_ids', NULL::bigint)),
                    (SELECT count(*) FROM nodes), 'kg_ids gives every node its own dense-id label');
SELECT assert.equal((warren.info('kg') #>> '{versions,0,edge_count}')::bigint,
                    2 * (SELECT (value)::bigint FROM truth WHERE key = 'edges'), 'the base version has every edge');

\set variant volume
\ir include/a2_ingest.sql
\set variant bridge
\ir include/a2_ingest.sql
\set variant novel
\ir include/a2_ingest.sql
