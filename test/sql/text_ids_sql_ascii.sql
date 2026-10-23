-- Text ids in a SQL_ASCII database (DESIGN.md §5.7, §14 item 42), where
-- any byte but NUL is valid text: ids that are not UTF-8 are mapped,
-- stored and returned as their bytes, unchanged.
CREATE EXTENSION IF NOT EXISTS dblink;
SELECT format('dbname=text_ids_ascii user=%s host=%s port=%s', session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
CREATE DATABASE text_ids_ascii ENCODING 'SQL_ASCII' TEMPLATE template0 LOCALE 'C';
SELECT dblink_connect('ascii', :'conninfo');
SELECT dblink_exec('ascii', $$
    CREATE EXTENSION pg_warren;
    -- café in Latin-1, two bytes that are no encoding's, and an ASCII id.
    CREATE TABLE ids AS SELECT * FROM (VALUES (1, E'caf\xe9'), (2, E'\xff\xfe'), (3, 'plain')) v(n, id);
    CREATE TABLE links AS SELECT a.id AS src, b.id AS dst FROM ids a JOIN ids b ON b.n = a.n % 3 + 1;
    DO $do$ BEGIN
        PERFORM warren.create_graph('g', 'SELECT id FROM ids', 'SELECT src, dst FROM links');
        PERFORM warren.refresh('g');
        PERFORM warren.vector_from_query('g', 'n', 'SELECT id, n FROM ids');
    END $do$
$$);

-- Each id's bytes in hex, so that they cross the connection unchanged.
SELECT assert.equal(found, 'fffe', 'the out-neighbour of café is the id of no encoding')
FROM dblink('ascii', $$SELECT string_agg(encode(convert_to(m, 'SQL_ASCII'), 'hex'), ',')
                       FROM warren.neighbours('g', E'caf\xe9'::text, 'out') m$$) AS t(found text);
SELECT assert.equal(found, '636166e9,706c61696e,fffe', 'id_map returns every id''s bytes, in byte order')
FROM dblink('ascii', $$SELECT string_agg(encode(convert_to(node_id, 'SQL_ASCII'), 'hex'), ',' ORDER BY dense_id)
                       FROM warren.id_map('g', NULL::text)$$) AS t(found text);
SELECT assert.equal(found, '706c61696e:3,fffe:2,636166e9:1', 'vectors are read by the same ids')
FROM dblink('ascii', $$SELECT string_agg(encode(convert_to(node_id, 'SQL_ASCII'), 'hex') || ':' || value, ','
                                        ORDER BY value DESC)
                       FROM warren.vector_values('g', 'n', NULL::text)$$) AS t(found text);
SELECT assert.equal(found, '1', 'a lookup by the bytes finds the node')
FROM dblink('ascii', $$SELECT warren.degree('g', E'\xff\xfe'::text, 'in')::text$$) AS t(found text);

SELECT dblink_disconnect('ascii');
DROP DATABASE text_ids_ascii;
