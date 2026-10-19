-- Self-check of the harness: the assertions in test/lib/assert.sql fail
-- when they should, so a passing test means something.
SELECT assert.that(true, 'that passes on true');
SELECT assert.equal(2::bigint, 2, 'equal passes on equal values');
SELECT assert.raises($$SELECT assert.that(false, 'x')$$, 'that fails on false', 'P0004');
SELECT assert.raises($$SELECT assert.that(NULL, 'x')$$, 'that fails on NULL', 'P0004');
SELECT assert.raises($$SELECT assert.equal(1, 2, 'x')$$, 'equal fails on different values', 'P0004');
SELECT assert.raises($$SELECT assert.equal(1, NULL, 'x')$$, 'equal fails against NULL', 'P0004');
SELECT assert.raises($$SELECT 1/0$$, 'raises matches the SQLSTATE', '22012');
SELECT assert.raises($$SELECT assert.raises('SELECT 1', 'x')$$, 'raises fails when nothing is raised', 'P0004');
SELECT assert.raises($$SELECT assert.raises('SELECT 1/0', 'x', '42P01')$$, 'raises fails on another SQLSTATE', 'P0004');
SELECT assert.equal(warren.version() IS NOT NULL, true, 'pg_warren is installed');
