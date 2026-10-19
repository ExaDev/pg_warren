-- Appendix A.2 / §8 at reduced scale: corpus ingest gated by structural
-- change, through the whole §8 workflow (ingest in a transaction, staged
-- refresh, diff, eigen on both versions, distance('subspace'), ROLLBACK),
-- for each ingest variant; then the shadow-schema pattern (a second graph,
-- compared by external id) gives the same scalars.
--
-- The robust finding at this scale: added volume barely turns the
-- community subspace, while a novel community or a bridge between two
-- turns about one of its dimensions away. Which of novel and bridge turns
-- further depends on the sizes (DESIGN.md §8), so only volume < both is
-- asserted. The ingest is larger than a community, as in A.2 (5,000 new
-- entities against communities of 1,250), so a novel community is among
-- the most cohesive.
-- fixture: evolving --communities 6 --community-size 40 --p-in 0.25 --p-out 0.004 --new-entities 80 --new-edges 1600 --seed 4
SELECT warren.create_graph('kg', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('kg') AS base \gset

\set variant volume
\ir include/a2_spectral.sql
\set d_volume :distance
\set variant novel
\ir include/a2_spectral.sql
\set d_novel :distance
\set variant bridge
\ir include/a2_spectral.sql
\set d_bridge :distance

SELECT :d_volume AS volume, :d_novel AS novel, :d_bridge AS bridge;
SELECT assert.that(:d_volume < 0.5, 'volume barely turns the subspace: ' || :d_volume);
SELECT assert.that(:d_novel > 2 * :d_volume, 'a novel community turns it further: ' || :d_novel);
SELECT assert.that(:d_bridge > 2 * :d_volume, 'a bridge turns it further: ' || :d_bridge);

-- The shadow-schema pattern: the bridge corpus in tables of its own, a
-- second graph over them, compared with kg's base by external id. The
-- shadow graph is built at once, so its dense ids are not kg's.
CREATE TABLE shadow_nodes AS SELECT id FROM nodes UNION ALL SELECT id FROM ingest_nodes WHERE variant = 'bridge';
CREATE TABLE shadow_edges AS SELECT src, dst FROM edges UNION ALL SELECT src, dst FROM ingest_edges WHERE variant = 'bridge';
SELECT warren.create_graph('kg_shadow', 'SELECT id FROM shadow_nodes', 'SELECT src, dst FROM shadow_edges',
                           directed := false);
SELECT warren.refresh('kg_shadow') AS shadow \gset
SELECT * FROM warren.eigen('kg', 6, 'normalized_laplacian', 'smallest', dtype := 'f64', tol := 1e-12,
                           vectors_prefix := 'lap', version := :base);
SELECT * FROM warren.eigen('kg_shadow', 6, 'normalized_laplacian', 'smallest', dtype := 'f64', tol := 1e-12,
                           vectors_prefix := 'lap');
SELECT warren.distance('kg', :base, :shadow, 'subspace', vectors_prefix := 'lap', k := 6,
                       graph2 := 'kg_shadow') AS cross \gset
SELECT assert.that(abs((:'cross'::jsonb ->> 'projection')::float8 - :d_bridge) < 1e-9,
                   'the shadow-schema comparison gives the same subspace distance');
SELECT assert.equal((warren.distance('kg', :base, :shadow, 'edit', graph2 := 'kg_shadow') #>> '{overall,removed}')::bigint,
                    0::bigint, 'by external id, the shadow graph only adds edges');
