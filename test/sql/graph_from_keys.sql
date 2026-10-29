-- Graphs from keys (DESIGN.md §9.3, §14 item 52): a shop's customers,
-- orders and products, order_items a junction of orders and products,
-- shipments keyed by (order_id, seq) and parcels referencing them, made a
-- graph by create_graph_from_keys, refreshed, traversed, and joined back
-- to the tables by their ids.

CREATE SCHEMA shop;
CREATE TABLE shop.regions (id int PRIMARY KEY);
CREATE TABLE shop.customers (id bigint PRIMARY KEY, name text NOT NULL,
                             region_id int REFERENCES shop.regions);
CREATE TABLE shop.orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES shop.customers);
CREATE TABLE shop.products (sku text PRIMARY KEY, title text NOT NULL);
CREATE TABLE shop.order_items (order_id bigint REFERENCES shop.orders,
                               sku text REFERENCES shop.products,
                               PRIMARY KEY (order_id, sku));
CREATE TABLE shop.shipments (order_id bigint REFERENCES shop.orders, seq int,
                             carrier text, PRIMARY KEY (order_id, seq));
CREATE TABLE shop.parcels (id bigint PRIMARY KEY, order_id bigint, seq int,
                           FOREIGN KEY (order_id, seq) REFERENCES shop.shipments);

INSERT INTO shop.regions VALUES (1);
INSERT INTO shop.customers VALUES (1, 'ada', 1), (2, 'bob', NULL), (3, 'cy', 1);
INSERT INTO shop.orders VALUES (10, 1), (11, 1), (12, 2), (13, NULL);
INSERT INTO shop.products VALUES ('pen', 'Pen'), ('ink', 'Ink'), ('a,b "c"', 'Odd');
INSERT INTO shop.order_items VALUES (10, 'pen'), (10, 'ink'), (11, 'ink'), (12, 'a,b "c"');
INSERT INTO shop.shipments VALUES (10, 1, 'post'), (10, 2, 'van'), (12, 1, 'post');
INSERT INTO shop.parcels VALUES (100, 10, 1), (101, 10, 2), (102, 10, 2), (103, NULL, NULL);

CREATE TABLE listed AS
SELECT ARRAY['shop.customers', 'shop.orders', 'shop.products', 'shop.order_items',
             'shop.shipments', 'shop.parcels']::regclass[] AS tables;

SELECT warren.create_graph_from_keys('shop', (SELECT tables FROM listed));
SELECT warren.refresh('shop');

SELECT assert.equal(warren.info('shop') ->> 'id_type', 'text', 'several tables have text ids');
SELECT assert.equal((warren.info('shop') ->> 'has_relations')::boolean, true, 'several relations');
SELECT assert.equal(warren.info('shop') ->> 'search_path', 'pg_catalog, pg_temp',
                    'the queries qualify every name, and run under the trusted search_path');
SELECT assert.equal(has_relations, true, 'queries_from_keys agrees')
FROM warren.queries_from_keys((SELECT tables FROM listed));

-- Each node table's rows are nodes; order_items, a junction, is not.
CREATE TABLE node_ids AS
SELECT 'shop.customers:' || id AS id FROM shop.customers
UNION ALL SELECT 'shop.orders:' || id FROM shop.orders
UNION ALL SELECT 'shop.products:' || sku FROM shop.products
UNION ALL SELECT 'shop.shipments:' || ROW(order_id, seq)::text FROM shop.shipments
UNION ALL SELECT 'shop.parcels:' || id FROM shop.parcels;
SELECT assert.equal((SELECT array_agg(node_id ORDER BY node_id COLLATE "C") FROM warren.id_map('shop', NULL::text)),
                    (SELECT array_agg(id ORDER BY id COLLATE "C") FROM node_ids),
                    'every node table row is a node, with its schema.table:key id');

-- Every edge, by relation, against the same joins written by hand: NULL
-- foreign keys (order 13, parcel 103) and the unlisted regions give none.
CREATE TABLE expected (src text, dst text, rel text);
INSERT INTO expected
SELECT 'shop.orders:' || o.id, 'shop.customers:' || o.customer_id, 'orders_customer_id_fkey'
FROM shop.orders o WHERE o.customer_id IS NOT NULL
UNION ALL
SELECT 'shop.orders:' || i.order_id, 'shop.products:' || i.sku, 'order_items' FROM shop.order_items i
UNION ALL
SELECT 'shop.shipments:' || ROW(s.order_id, s.seq)::text, 'shop.orders:' || s.order_id,
       'shipments_order_id_fkey'
FROM shop.shipments s
UNION ALL
SELECT 'shop.parcels:' || p.id, 'shop.shipments:' || ROW(p.order_id, p.seq)::text, 'parcels_order_id_seq_fkey'
FROM shop.parcels p WHERE p.order_id IS NOT NULL;
SELECT assert.equal(
    (SELECT array_agg(e ORDER BY e COLLATE "C")
     FROM (SELECT DISTINCT src, rel FROM expected) s,
          warren.neighbours('shop', s.src, rels := ARRAY[s.rel]) n,
          LATERAL (SELECT s.rel || ' ' || s.src || ' > ' || n) AS x (e)),
    (SELECT array_agg(e ORDER BY e COLLATE "C")
     FROM expected, LATERAL (SELECT rel || ' ' || src || ' > ' || dst) AS x (e)),
    'each foreign key and the junction are a relation, src the referencing row');
SELECT assert.equal((warren.info('shop') -> 'versions' -> 0 ->> 'edge_count')::bigint,
                    (SELECT count(*) FROM expected), 'no other edges');
SELECT assert.equal((warren.info('shop') -> 'versions' -> 0 ->> 'dropped_edges')::bigint, 0::bigint,
                    'no edge is dropped');

-- What ada's orders hold, two steps out, joined back to the products.
SELECT assert.equal(
    (SELECT array_agg(p.title ORDER BY p.title)
     FROM warren.bfs('shop', ARRAY['shop.customers:1'], 2, direction := 'both') b
     JOIN shop.products p ON 'shop.products:' || p.sku = b.node_id),
    ARRAY['Ink', 'Pen'], 'bfs reaches the products of ada''s orders');
-- The composite key's id is a row literal, which quotes nothing here.
SELECT assert.equal(
    (SELECT array_agg(s.carrier ORDER BY s.carrier)
     FROM warren.neighbours('shop', 'shop.parcels:101'::text) n
     JOIN shop.shipments s ON 'shop.shipments:' || ROW(s.order_id, s.seq)::text = n),
    ARRAY['van'], 'a parcel links to its shipment');
-- A key holding a comma, a space and quotes is an id as it is.
SELECT assert.equal(
    (SELECT array_agg(n) FROM warren.neighbours('shop', 'shop.orders:12'::text, rels := ARRAY['order_items']) n),
    ARRAY['shop.products:a,b "c"'], 'an odd key is its own text');

-- One table of nodes keeps its own key as the id: customers alone, or
-- with a junction of its own rows.
CREATE TABLE shop.follows (src bigint REFERENCES shop.customers, dst bigint REFERENCES shop.customers,
                           PRIMARY KEY (src, dst));
INSERT INTO shop.follows VALUES (1, 2), (2, 3);
SELECT warren.create_graph_from_keys('people', ARRAY['shop.customers', 'shop.follows']::regclass[]);
SELECT warren.refresh('people');
SELECT assert.equal(warren.info('people') ->> 'id_type', 'bigint', 'one table keeps bigint ids');
SELECT assert.equal(warren.shortest_path('people', 1::bigint, 3::bigint), ARRAY[1, 2, 3]::bigint[],
                    'follows is an edge per row');

-- Refusals.
CREATE TABLE shop.events (at timestamptz);
SELECT assert.raises($$SELECT warren.create_graph_from_keys('x', ARRAY['shop.events']::regclass[])$$,
                     'a table without a primary key', '55000');
SELECT assert.raises($$SELECT warren.queries_from_keys('{}')$$, 'no tables', '22023');
SELECT assert.raises($$SELECT warren.create_graph_from_keys('shop', ARRAY['shop.orders']::regclass[])$$,
                     'graph names are unique', '42710');

-- A role must read every listed table, as create_graph checks.
CREATE ROLE gk_clerk;
GRANT USAGE ON SCHEMA shop, assert TO gk_clerk;
GRANT SELECT ON shop.customers, shop.orders TO gk_clerk;
SET ROLE gk_clerk;
SELECT assert.raises($$SELECT warren.create_graph_from_keys('clerk', ARRAY['shop.orders', 'shop.products']::regclass[])$$,
                     'without SELECT on a listed table', '42501');
SELECT warren.create_graph_from_keys('clerk', ARRAY['shop.customers', 'shop.orders']::regclass[]);
SELECT assert.equal(warren.info('clerk') ->> 'owner', 'gk_clerk', 'the caller owns the graph');
RESET ROLE;
