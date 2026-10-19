-- Assertions for SQL tests (test/README.md). Each raises assert_failure
-- (P0004) when it does not hold, which fails the test under ON_ERROR_STOP.
CREATE SCHEMA assert;

CREATE FUNCTION assert.that(condition boolean, message text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
  IF condition IS NOT TRUE THEN
    RAISE EXCEPTION 'assertion failed: %', message USING ERRCODE = 'assert_failure';
  END IF;
END $$;

CREATE FUNCTION assert.equal(actual anycompatible, expected anycompatible, message text) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
  IF actual IS DISTINCT FROM expected THEN
    RAISE EXCEPTION 'assertion failed: %: expected %, got %', message, expected, actual
      USING ERRCODE = 'assert_failure';
  END IF;
END $$;

-- Passes when `statement` raises an error (with SQLSTATE `expected_sqlstate`,
-- if given).
CREATE FUNCTION assert.raises(statement text, message text, expected_sqlstate text DEFAULT NULL) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE raised text;
BEGIN
  BEGIN
    EXECUTE statement;
  EXCEPTION WHEN OTHERS OR assert_failure THEN  -- OTHERS excludes assert_failure
    raised := SQLSTATE;
  END;
  IF raised IS NULL OR raised IS DISTINCT FROM coalesce(expected_sqlstate, raised) THEN
    RAISE EXCEPTION 'assertion failed: %: expected % from %, got %', message,
      coalesce('SQLSTATE ' || expected_sqlstate, 'an error'), statement, coalesce('SQLSTATE ' || raised, 'no error')
      USING ERRCODE = 'assert_failure';
  END IF;
END $$;
