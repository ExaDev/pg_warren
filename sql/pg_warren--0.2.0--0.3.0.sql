-- pg_warren 0.2.0 -> 0.3.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.2.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.2.0 yields the catalog of a fresh
-- 0.3.0 install.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.3.0'" to load this file. \quit
