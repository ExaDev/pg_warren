-- DESIGN.md §7: create_graph raises a NOTICE when the graph's search_path
-- has a schema that roles other than its owner can create objects in, and
-- none otherwise. The NOTICE goes to the client, which a test cannot read,
-- so it is also logged (log_min_messages) and read back from the server log.
SET log_min_messages = notice;
CREATE ROLE sp_other;
CREATE SCHEMA sp_shared;
GRANT CREATE ON SCHEMA sp_shared TO sp_other;
CREATE SCHEMA sp_private;
CREATE TABLE sp_private.nodes AS SELECT generate_series(1, 3)::bigint AS id;
CREATE TABLE sp_private.edges AS SELECT 1::bigint AS src, 2::bigint AS dst;

SET search_path = sp_shared, sp_private;
SELECT warren.create_graph('sp_flagged', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SET search_path = sp_private;
SELECT warren.create_graph('sp_quiet', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
RESET search_path;

SELECT pg_read_file(current_setting('data_directory') || '/../server.log') AS log \gset
SELECT assert.that(position('NOTICE:  graph "sp_flagged" runs its source queries under search_path "sp_shared, sp_private", in which other roles can create objects (in sp_shared)' in :'log') > 0,
                   'create_graph names the writable schema');
SELECT assert.that(position('graph "sp_quiet"' in :'log') = 0,
                   'and says nothing when only the owner can write the path');
