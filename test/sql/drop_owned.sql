-- DESIGN.md §5.7, §14 items 44 and 45: dropping a role that owns graphs,
-- reads others' and wrote vectors on them, in committed transactions as an
-- administrator would. Without the event trigger, Postgres's own procedure
-- (REASSIGN OWNED, DROP OWNED, DROP ROLE) moves and drops no graph, so the
-- graphs are orphaned until warren.drop_orphans(). With the trigger (created
-- by the superuser this test runs as), DROP OWNED is refused while the role
-- owns graphs, and otherwise cleans up after it; warren.reassign_owned and
-- warren.drop_owned work with or without it.
GRANT USAGE ON SCHEMA assert TO PUBLIC;
CREATE ROLE do_alice;
CREATE ROLE do_bob;
CREATE ROLE do_carol;
CREATE ROLE do_dave;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
GRANT SELECT ON nodes, edges TO do_alice, do_bob, do_carol;

SET ROLE do_alice;
SELECT warren.create_graph('a', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('a');
RESET ROLE;
SET ROLE do_bob;
SELECT warren.create_graph('b', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('b');
SELECT warren.vector_const('b', 'bobs', 1);
SELECT warren.grant('b', 'do_alice');
SELECT warren.grant('b', 'do_carol');
SELECT warren.grant('b', 'do_dave');
RESET ROLE;
SET ROLE do_alice;
SELECT warren.vector_const('b', 'alices', 2);
RESET ROLE;
SET ROLE do_dave;
SELECT warren.vector_const('b', 'daves', 3);
RESET ROLE;

CREATE VIEW ownership AS
SELECT coalesce((SELECT string_agg(name || ':' || owner::text || ':' || readers::text, ' ' ORDER BY name)
                 FROM warren.graphs), '')
       || ' / ' ||
       coalesce((SELECT string_agg(g.name || '.' || v.name || ':' || v.created_by::text, ' '
                                   ORDER BY g.name, v.name)
                 FROM warren.vectors v JOIN warren.graphs g USING (graph_id)), '') AS held;

-- Without the trigger: the standard procedure leaves every graph as it was.
REASSIGN OWNED BY do_alice TO do_bob;
DROP OWNED BY do_alice;
SELECT assert.equal((SELECT held FROM ownership),
    'a:do_alice:{} b:do_bob:{do_alice,do_carol,do_dave} / b.alices:do_alice b.bobs:do_bob b.daves:do_dave',
    'REASSIGN OWNED and DROP OWNED move and drop no graph');
SELECT 'do_alice'::regrole::oid AS alice \gset
DROP ROLE do_alice;

-- The sweep.
SELECT assert.equal((SELECT string_agg(kind || ':' || graph || ':' || (role = :alice), ' ' ORDER BY kind)
                     FROM warren.orphans()),
                    'graph:a:true reader:b:true vector:b:true', 'orphans lists what names do_alice');
SELECT assert.raises($$SELECT warren.info('a')$$, 'an orphan fails closed', '42704');
SET ROLE do_bob;
SELECT assert.equal((SELECT string_agg(kind || ':' || graph, ' ' ORDER BY kind) FROM warren.orphans()),
                    'reader:b vector:b', 'an owner sees what dangles from its own graphs');
SELECT assert.raises($$SELECT warren.drop_orphans()$$, 'an owner may not sweep', '42501');
RESET ROLE;
SELECT assert.equal(warren.drop_orphans(), 1::bigint, 'drop_orphans drops the orphaned graph');
SELECT assert.equal((SELECT count(*) FROM warren.orphans()), 0::bigint, 'nothing is left orphaned');
SELECT assert.equal((SELECT held FROM ownership),
    'b:do_bob:{do_carol,do_dave} / b.bobs:do_bob b.daves:do_dave', 'the rest is pruned');

-- The functions, without the trigger.
CREATE ROLE do_erin;
SET ROLE do_erin;
SELECT warren.create_graph('e', 'SELECT 1::bigint', 'SELECT 1::bigint, 1::bigint');
RESET ROLE;
SET ROLE do_carol;
SELECT assert.raises($$SELECT warren.drop_owned('do_erin')$$,
                     'drop_owned needs the privileges of the role', '42501');
RESET ROLE;
SELECT assert.equal(warren.drop_owned('do_erin'), 1::bigint, 'drop_owned works without the trigger');
DROP ROLE do_erin;

-- With the trigger.
CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
    WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned();

-- §14 item 44a: the standard procedure is refused while do_bob owns b,
-- and the refusal names the graph and both functions.
REASSIGN OWNED BY do_bob TO do_carol;
DO $$
BEGIN
  DROP OWNED BY do_bob;
  RAISE EXCEPTION 'DROP OWNED was not refused' USING ERRCODE = 'assert_failure';
EXCEPTION WHEN dependent_objects_still_exist THEN
  PERFORM assert.that(SQLERRM LIKE '%role "do_bob" while it owns graph "b"%warren.reassign_owned%warren.drop_owned%',
                      'the refusal names the graph and the functions: ' || SQLERRM);
END $$;
SELECT assert.equal((SELECT held FROM ownership),
    'b:do_bob:{do_carol,do_dave} / b.bobs:do_bob b.daves:do_dave', 'the refused DROP OWNED changed nothing');
SELECT assert.raises($$DROP ROLE do_bob$$, 'do_bob still holds its table privileges', '2BP01');

SELECT assert.equal(warren.reassign_owned('do_bob', 'do_carol'), 1::bigint, 'reassign_owned moves b');
DROP OWNED BY do_bob;
DROP ROLE do_bob;
SELECT assert.equal((SELECT held FROM ownership),
    'b:do_carol:{do_dave} / b.bobs:do_carol b.daves:do_dave', 'b and its vector are do_carol''s');
SET ROLE do_carol;
SELECT assert.that(warren.refresh('b') IS NOT NULL, 'the new owner refreshes b');
RESET ROLE;

-- A role that owns no graph: DROP OWNED removes its reader entries and
-- vectors.
DROP OWNED BY do_dave;
DROP ROLE do_dave;
SELECT assert.equal((SELECT held FROM ownership), 'b:do_carol:{} / b.bobs:do_carol',
                    'DROP OWNED removed do_dave''s reader entry and vector');

-- A role whose privileges the caller lacks is left to DROP OWNED's error.
CREATE ROLE do_stranger;
SET ROLE do_stranger;
SELECT assert.raises($$DROP OWNED BY do_carol$$, 'DROP OWNED by a stranger', '42501');
RESET ROLE;

-- drop_owned with the trigger, then the whole procedure.
SELECT assert.equal(warren.drop_owned('do_carol'), 1::bigint, 'drop_owned drops b');
DROP OWNED BY do_carol;
DROP ROLE do_carol;
DROP ROLE do_stranger;
SELECT assert.equal((SELECT held FROM ownership), ' / ', 'nothing is left');
SELECT assert.equal((SELECT count(*) FROM warren.orphans()), 0::bigint, 'and nothing is orphaned');

-- The trigger depends on the extension's function.
SELECT assert.raises($$DROP EXTENSION pg_warren$$, 'DROP EXTENSION needs CASCADE', '2BP01');
DROP EXTENSION pg_warren CASCADE;
SELECT assert.equal((SELECT count(*) FROM pg_event_trigger WHERE evtname = 'warren_drop_owned'), 0::bigint,
                    'CASCADE drops the trigger');
