-- DESIGN.md §7 and §13 M7: a trusted install. A non-superuser that owns
-- the database runs CREATE EXTENSION. The script then runs as the bootstrap
-- superuser, so that role owns the warren.* tables, while
-- pg_extension.extowner is the installer. Catalog statements must elevate to
-- the tables' owner and never to the installer. Neither the installer nor
-- another non-superuser may reach the catalog, or another role's graph,
-- except through the warren.* functions and their access checks.
GRANT USAGE ON SCHEMA assert TO PUBLIC;
DROP EXTENSION pg_warren;
CREATE ROLE trusted_installer;
CREATE ROLE trusted_owner;
ALTER DATABASE :"DBNAME" OWNER TO trusted_installer;

SET ROLE trusted_installer;
CREATE EXTENSION pg_warren;
RESET ROLE;

SELECT assert.equal((SELECT extowner::regrole::text FROM pg_extension WHERE extname = 'pg_warren'),
                    'trusted_installer', 'the installer owns the extension');
SELECT assert.equal((SELECT array_agg(DISTINCT c.relowner) FROM pg_class c
                     WHERE c.relnamespace = 'warren'::regnamespace),
                    ARRAY[10::oid], 'the bootstrap superuser owns every warren relation');
SELECT assert.equal((SELECT nspowner FROM pg_namespace WHERE nspname = 'warren'), 10::oid,
                    'the bootstrap superuser owns schema warren');
SELECT assert.equal(
    (SELECT count(*) FROM pg_class c, unnest(ARRAY['trusted_installer', 'trusted_owner']) r
     WHERE c.relnamespace = 'warren'::regnamespace
       AND CASE c.relkind
             WHEN 'S' THEN has_sequence_privilege(r, c.oid, 'USAGE, SELECT, UPDATE')
             WHEN 'r' THEN has_table_privilege(r, c.oid,
                             'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
             ELSE false END),
    0::bigint, 'neither non-superuser has any privilege on a warren table or sequence');

-- Fails unless it runs as `role`, and that role is not a superuser: source,
-- message and value queries and executor-fallback expressions run as the
-- graph owner or the caller, never elevated (§7).
CREATE FUNCTION public.must_run_as(role text, x bigint) RETURNS bigint
LANGUAGE plpgsql AS $$
BEGIN
  IF current_user <> role OR (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) THEN
    RAISE EXCEPTION 'ran as %, not %', current_user, role;
  END IF;
  RETURN x;
END $$;

-- A second non-superuser's graph, over tables the installer cannot read.
CREATE TABLE public.secret_nodes AS SELECT generate_series(1, 6)::bigint AS id;
CREATE TABLE public.secret_edges AS SELECT id AS src, id % 6 + 1 AS dst FROM public.secret_nodes;
REVOKE ALL ON public.secret_nodes, public.secret_edges FROM PUBLIC;
GRANT SELECT ON public.secret_nodes, public.secret_edges TO trusted_owner;

SET ROLE trusted_owner;
SELECT warren.create_graph('secret',
    $$SELECT public.must_run_as('trusted_owner', id) FROM public.secret_nodes$$,
    $$SELECT public.must_run_as('trusted_owner', src), dst FROM public.secret_edges$$);
SELECT assert.that(warren.refresh('secret') IS NOT NULL, 'a non-superuser owner refreshes');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('secret', ARRAY[1::bigint], 6)), 6::bigint,
                    'a non-superuser owner traverses');
SELECT assert.that(warren.vector_from_query('secret', 'from_query',
    $$SELECT id, public.must_run_as('trusted_owner', id) FROM public.secret_nodes$$) IS NOT NULL,
    'a value query runs as the caller');
SELECT warren.vector_dense_id('secret', 'i');
SELECT assert.equal(warren.explain_map('secret', $$public.must_run_as('trusted_owner', i)$$) ->> 'path',
                    'executor', 'a user function takes the executor fallback');
SELECT warren.vector_map('secret', 'mapped', $$public.must_run_as('trusted_owner', i)$$);
SELECT assert.equal(warren.vector_reduce('secret', 'mapped', 'sum'), 15::float8,
                    'the executor fallback runs as the caller');
SELECT assert.equal((SELECT count(*) FROM warren.gather('secret',
    $$SELECT id, public.must_run_as('trusted_owner', id) FROM public.secret_nodes$$,
    NULL::bigint, NULL::bigint, 'sum')), 6::bigint, 'a message query runs as the caller');
SELECT assert.that(warren.pagerank('secret', 'rank') > 0, 'the library runs for a non-superuser');
RESET ROLE;

-- A superuser refreshing it still runs the source queries as the graph owner.
SELECT assert.that(warren.refresh('secret') IS NOT NULL, 'a superuser refreshes as the owner');

-- The installer owns the extension and the database, but not the catalog.
SET ROLE trusted_installer;
SELECT assert.raises($$SELECT count(*) FROM warren.graphs$$, 'installer reads graphs', '42501');
SELECT assert.raises($$SELECT count(*) FROM warren.segments$$, 'installer reads segments', '42501');
SELECT assert.raises($$SELECT count(*) FROM warren.vector_chunks$$, 'installer reads vectors', '42501');
SELECT assert.raises($$UPDATE warren.graphs SET node_query = 'SELECT 1::bigint'$$,
                     'installer rewrites a source query', '42501');
SELECT assert.raises($$INSERT INTO warren.graphs (name, owner, search_path, id_type)
                       VALUES ('planted', 'trusted_owner', 'public', 'int8')$$,
                     'installer plants a graph', '42501');
SELECT assert.raises($$SELECT nextval('warren.vectors_vector_id_seq')$$,
                     'installer uses a catalog sequence', '42501');
SELECT assert.raises($$ALTER TABLE warren.graphs OWNER TO trusted_installer$$,
                     'installer takes the catalog', '42501');
SELECT assert.raises($$ALTER TABLE warren.graphs ENABLE ROW LEVEL SECURITY$$,
                     'installer alters the catalog', '42501');
SELECT assert.raises($$ALTER EXTENSION pg_warren DROP TABLE warren.graphs$$,
                     'installer detaches the catalog', '42501');
SELECT assert.raises($$ALTER EXTENSION pg_warren ADD TABLE public.secret_nodes$$,
                     'installer adds another role''s table to the extension', '42501');
SELECT assert.raises($$CREATE FUNCTION warren.planted() RETURNS int LANGUAGE sql AS 'SELECT 1'$$,
                     'installer creates in schema warren', '42501');
CREATE FUNCTION public.planted_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RETURN NEW; END $$;
-- As the extension's owner it may add an object it owns to the extension
-- (DESIGN.md §7). That is harmless: the object keeps its owner and
-- privileges, gains none on the catalog, and goes with DROP EXTENSION.
ALTER EXTENSION pg_warren ADD FUNCTION public.planted_trigger();
SELECT assert.equal((SELECT proowner::regrole::text FROM pg_proc WHERE proname = 'planted_trigger'),
                    'trusted_installer', 'an added function keeps its owner');
SELECT assert.raises($$CREATE TRIGGER planted BEFORE INSERT ON warren.vectors
                       FOR EACH ROW EXECUTE FUNCTION public.planted_trigger()$$,
                     'installer adds a trigger to the catalog', '42501');
SELECT assert.raises($$SELECT warren.info('secret')$$, 'installer reads another role''s graph', '42501');
SELECT assert.raises($$SELECT count(*) FROM warren.bfs('secret', ARRAY[1::bigint], 6)$$,
                     'installer traverses another role''s graph', '42501');
SELECT assert.raises($$SELECT warren.refresh('secret')$$, 'installer refreshes another role''s graph', '42501');
SELECT assert.raises($$SELECT warren.grant('secret', 'trusted_installer')$$,
                     'installer grants itself another role''s graph', '42501');
SELECT assert.raises($$SELECT warren.drop_graph('secret')$$, 'installer drops another role''s graph', '42501');
SELECT assert.raises($$SELECT warren.drop_vector('secret', 'rank')$$,
                     'installer drops another role''s vector', '42501');

-- Its own graph works: a non-superuser caller's catalog writes elevate.
CREATE TABLE public.mine_nodes AS SELECT generate_series(1, 3)::bigint AS id;
CREATE TABLE public.mine_edges AS SELECT 1::bigint AS src, 2::bigint AS dst;
SELECT warren.create_graph('mine', 'SELECT id FROM public.mine_nodes', 'SELECT src, dst FROM public.mine_edges');
SELECT assert.that(warren.refresh('mine') IS NOT NULL, 'the installer refreshes its own graph');
SELECT assert.equal((SELECT count(*) FROM warren.neighbours('mine', 1::bigint)), 1::bigint,
                    'the installer queries its own graph');
RESET ROLE;

-- The graph owner cannot reach the catalog around the functions either,
-- nor the installer's graph.
SET ROLE trusted_owner;
SELECT assert.raises($$SELECT count(*) FROM warren.segments$$, 'owner reads segments', '42501');
SELECT assert.raises($$UPDATE warren.graphs SET owner = 'trusted_owner'$$, 'owner takes graphs', '42501');
SELECT assert.raises($$SELECT warren.info('mine')$$, 'owner reads the installer''s graph', '42501');
RESET ROLE;

-- Granting reader access still works across the two non-superusers.
SET ROLE trusted_owner;
SELECT warren.grant('secret', 'trusted_installer');
RESET ROLE;
SET ROLE trusted_installer;
SELECT assert.equal((SELECT count(*) FROM warren.bfs('secret', ARRAY[1::bigint], 6)), 6::bigint,
                    'a granted reader traverses');
SELECT assert.raises($$SELECT warren.refresh('secret')$$, 'a reader cannot refresh', '42501');
RESET ROLE;

-- DESIGN.md §5.7: dropping a role needs no superuser and no event trigger.
-- A non-superuser with a leaving role's privileges reassigns or drops its
-- graphs before DROP OWNED and DROP ROLE.
CREATE ROLE trusted_leaver;
CREATE ROLE trusted_quitter;
GRANT SELECT ON public.secret_nodes, public.secret_edges TO trusted_leaver, trusted_quitter;
GRANT trusted_leaver, trusted_quitter TO trusted_owner;
SET ROLE trusted_leaver;
SELECT warren.create_graph('leavers', 'SELECT id FROM public.secret_nodes', 'SELECT src, dst FROM public.secret_edges');
RESET ROLE;
SET ROLE trusted_quitter;
SELECT warren.create_graph('quitters', 'SELECT id FROM public.secret_nodes', 'SELECT src, dst FROM public.secret_edges');
RESET ROLE;
SET ROLE trusted_owner;
SELECT assert.equal(warren.reassign_owned('trusted_leaver', 'trusted_owner'), 1::bigint,
                    'a non-superuser reassigns a leaving role''s graph');
SELECT assert.equal(warren.drop_owned('trusted_quitter'), 1::bigint,
                    'a non-superuser drops a leaving role''s graph');
RESET ROLE;
DROP OWNED BY trusted_leaver, trusted_quitter;
DROP ROLE trusted_leaver, trusted_quitter;
SELECT assert.equal((SELECT string_agg(name || ':' || owner::text, ' ' ORDER BY name) FROM warren.graphs
                     WHERE name IN ('leavers', 'quitters')), 'leavers:trusted_owner', 'one moved, one dropped');

-- §14 item 45: a graph whose owner was dropped without any of that may be
-- dropped by the extension's owner, the installer, but only a superuser
-- may adopt it.
CREATE ROLE trusted_vanished;
GRANT SELECT ON public.secret_nodes, public.secret_edges TO trusted_vanished;
SET ROLE trusted_vanished;
SELECT warren.create_graph('vanished', 'SELECT id FROM public.secret_nodes', 'SELECT src, dst FROM public.secret_edges');
SELECT warren.create_graph('adopted', 'SELECT id FROM public.secret_nodes', 'SELECT src, dst FROM public.secret_edges');
RESET ROLE;
REVOKE ALL ON public.secret_nodes, public.secret_edges FROM trusted_vanished;
DROP ROLE trusted_vanished;
SET ROLE trusted_owner;
SELECT assert.equal((SELECT count(*) FROM warren.orphans() WHERE kind = 'graph'), 0::bigint,
                    'another role does not see orphaned graphs');
SELECT assert.raises($$SELECT warren.drop_graph('vanished')$$, 'another role drops an orphan', '42704');
SELECT assert.raises($$SELECT warren.drop_orphans()$$, 'another role sweeps', '42501');
RESET ROLE;
SET ROLE trusted_installer;
SELECT assert.equal((SELECT string_agg(graph, ' ' ORDER BY graph) FROM warren.orphans() WHERE kind = 'graph'),
                    'adopted vanished', 'the installer sees the orphaned graphs');
SELECT assert.raises($$SELECT warren.set_owner('adopted', 'trusted_installer')$$,
                     'the installer adopts an orphan', '42501');
SELECT warren.drop_graph('vanished');
RESET ROLE;
SELECT warren.set_owner('adopted', 'trusted_owner');
SELECT assert.equal((SELECT owner::text FROM warren.graphs WHERE name = 'adopted'), 'trusted_owner',
                    'a superuser adopts an orphan');
SELECT assert.equal((SELECT count(*) FROM warren.orphans()), 0::bigint, 'nothing is left orphaned');

-- The event trigger is a superuser's opt-in: the installer cannot create
-- it, and while it exists the installer's DROP EXTENSION needs CASCADE.
SET ROLE trusted_installer;
SELECT assert.raises($$CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
                       WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned()$$,
                     'the installer creates the event trigger', '42501');
RESET ROLE;
SELECT assert.equal((SELECT count(*) FROM pg_event_trigger), 0::bigint,
                    'the extension created no event trigger');
CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
    WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned();
SET ROLE trusted_installer;
SELECT assert.raises($$DROP EXTENSION pg_warren$$, 'DROP EXTENSION with the trigger', '2BP01');
-- As the extension's owner it can still drop the extension, and with it
-- every graph (documented in DESIGN.md §7), and the trigger with CASCADE,
-- which does not check who owns what it drops.
DROP EXTENSION pg_warren CASCADE;
RESET ROLE;
SELECT assert.equal((SELECT count(*) FROM pg_event_trigger), 0::bigint, 'CASCADE drops the trigger');
SELECT assert.equal((SELECT count(*) FROM pg_namespace WHERE nspname = 'warren'), 0::bigint,
                    'dropping the extension drops the catalog');
SELECT assert.equal((SELECT count(*) FROM pg_proc WHERE proname = 'planted_trigger'), 0::bigint,
                    'a function added to the extension is dropped with it');
