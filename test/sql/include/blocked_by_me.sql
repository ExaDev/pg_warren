-- assert_blocked_by_me(pid, what): waits, up to 10 s, until backend pid is
-- blocked by this session (pg_blocking_pids), and fails naming what if it
-- never is. dblink_is_busy after a sleep is also true before the other
-- session has reached the lock, so a race test that relied on it could
-- pass without testing the path after the lock.
CREATE FUNCTION assert_blocked_by_me(pid int, what text) RETURNS void
LANGUAGE plpgsql AS $f$
BEGIN
    FOR i IN 1..200 LOOP
        IF pg_catalog.pg_backend_pid() = ANY (pg_catalog.pg_blocking_pids(pid)) THEN
            RETURN;
        END IF;
        PERFORM pg_catalog.pg_sleep(0.05);
    END LOOP;
    RAISE EXCEPTION 'assertion failed: % never waited on this session', what;
END
$f$;
