-- One case of dropped_roles_race.sql: this session runs :first in an open
-- transaction, session b runs :call, which must wait on this session, and
-- then this session commits. Sets :b_result (b's rows, comma-separated,
-- empty if it failed) and :b_error (its error message, or OK).
BEGIN;
:first;
SELECT dblink_send_query('b', :'call');
SELECT assert_blocked_by_me(:b_pid, :'call');
COMMIT;
SELECT coalesce(string_agg(x, ','), '') AS b_result FROM dblink_get_result('b', false) AS t(x text) \gset
SELECT dblink_error_message('b') AS b_error \gset
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query
