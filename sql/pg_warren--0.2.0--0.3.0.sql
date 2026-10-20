-- pg_warren 0.2.0 -> 0.3.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.2.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.2.0 yields the catalog of a fresh
-- 0.3.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.3.0'" to load this file. \quit

-- A dump must restore (DESIGN.md §5.1): break the two foreign-key cycles
-- among the catalog tables, which pg_dump cannot order.
ALTER TABLE warren.graphs DROP CONSTRAINT graphs_current_version_fkey;
ALTER TABLE warren.versions DROP CONSTRAINT versions_label_vector_fkey;

-- graphs.current_version is not a foreign key: with versions.graph_id it
-- would make a cycle, and pg_dump cannot order the data of tables in a
-- foreign-key cycle, so a dump would not restore (DESIGN.md §5.1). These
-- triggers keep what that key kept, and more: the current version is one of
-- the graph's own versions, and cannot be deleted while it is current. They
-- never fire on the inserts a restore makes. (versions.label_vector, for
-- contraction in v1.1, has no foreign key for the same reason.)
CREATE FUNCTION warren.current_version_is_the_graphs() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    -- FOR KEY SHARE, as a foreign key would: a concurrent delete of the
    -- version waits for this transaction.
    IF NEW.current_version IS NOT NULL AND NOT EXISTS (
        SELECT FROM warren.versions v
        WHERE v.version_id = NEW.current_version AND v.graph_id = NEW.graph_id
        FOR KEY SHARE) THEN
        RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
            MESSAGE = format('version %s is not a version of graph "%s"', NEW.current_version, NEW.name);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER current_version_is_the_graphs BEFORE UPDATE OF current_version ON warren.graphs
    FOR EACH ROW EXECUTE FUNCTION warren.current_version_is_the_graphs();

CREATE FUNCTION warren.current_version_is_kept() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF EXISTS (SELECT FROM warren.graphs g WHERE g.current_version = OLD.version_id) THEN
        RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
            MESSAGE = format('version %s is the current version of its graph', OLD.version_id);
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER current_version_is_kept BEFORE DELETE ON warren.versions
    FOR EACH ROW EXECUTE FUNCTION warren.current_version_is_kept();
