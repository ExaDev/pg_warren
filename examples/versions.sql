-- Keep the whole what-if scenario inside one transaction, then undo it.
BEGIN;
INSERT INTO warren_examples.people VALUES (10001, 'new-person');
INSERT INTO warren_examples.follows VALUES (1, 10001);

-- The current graph still sees the original 10000 people.
SELECT count(*) AS current_reachable
FROM warren.bfs('example_social', ARRAY[1::bigint], NULL::integer);

DO $$
DECLARE trial bigint; current_id bigint; reachable bigint;
BEGIN
    current_id := (warren.info('example_social') ->> 'current_version')::bigint;
    trial := warren.refresh('example_social', promote := false, label := 'example-trial');
    SELECT count(*) INTO reachable
    FROM warren.bfs('example_social', ARRAY[1::bigint], NULL::integer, version := trial);
    RAISE NOTICE 'Staged version reaches % people', reachable;
    RAISE NOTICE 'Difference: %', warren.diff('example_social', current_id, trial);
END $$;
ROLLBACK;

SELECT count(*) AS source_rows_after_rollback FROM warren_examples.people;
SELECT count(*) AS reachable_after_rollback
FROM warren.bfs('example_social', ARRAY[1::bigint], NULL::integer);
