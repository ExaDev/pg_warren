#!/usr/bin/env python3
"""Writes a seeded, deterministic fixture graph to a directory: one CSV per
table, `truth.json`/`truth.csv` (ground truth for assertions) and `load.sql`
(run it with psql from inside that directory). Defaults are the scales of
DESIGN.md Appendix A; pass smaller parameters for tests.

    bench/generate.py sbm --communities 4 --community-size 50 --out /tmp/sbm
    (cd /tmp/sbm && psql -v ON_ERROR_STOP=1 -f load.sql)
"""

import argparse
import sys

from fixtures import citation, entity_resolution, evolving, fraud, grid, power_law, random_graph, sbm

A5_RELATIONS = ("related_to", "part_of", "mentions")


def _ints(text):
    return tuple(int(x) for x in text.split(","))


def _names(text):
    return tuple(x for x in text.split(",") if x)


def _block_model(parser, embedding_dim):
    parser.add_argument("--communities", type=int, default=40)
    parser.add_argument("--community-size", type=int, default=1250)
    parser.add_argument("--p-in", type=float, default=0.05)
    parser.add_argument("--p-out", type=float, default=0.0005)
    parser.add_argument("--embedding-dim", type=int, default=embedding_dim)
    parser.add_argument("--noise", type=float, default=0.5)


def _block_args(a):
    return dict(communities=a.communities, community_size=a.community_size, p_in=a.p_in, p_out=a.p_out,
                seed=a.seed, embedding_dim=a.embedding_dim, noise=a.noise)


def parser():
    root = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    kinds = root.add_subparsers(dest="kind", required=True)

    def kind(name, help, run):
        p = kinds.add_parser(name, help=help)
        p.add_argument("--out", required=True, help="directory to write the fixture to")
        p.add_argument("--seed", type=int, default=1)
        p.set_defaults(run=run)
        return p

    p = kind("random", "Erdős–Rényi G(n, m)",
             lambda a: random_graph.generate(a.nodes, a.edges, a.seed, a.directed))
    p.add_argument("--nodes", type=int, default=10_000)
    p.add_argument("--edges", type=int, default=50_000)
    p.add_argument("--directed", action="store_true")

    p = kind("power-law", "preferential attachment (Barabási–Albert)",
             lambda a: power_law.generate(a.nodes, a.attachments, a.seed))
    p.add_argument("--nodes", type=int, default=10_000)
    p.add_argument("--attachments", type=int, default=5)

    p = kind("grid", "rows × cols lattice", lambda a: grid.generate(a.rows, a.cols))
    p.add_argument("--rows", type=int, default=100)
    p.add_argument("--cols", type=int, default=100)

    p = kind("sbm", "stochastic block model (A.2 base)",
             lambda a: sbm.generate(**_block_args(a), relations=a.relations))
    _block_model(p, embedding_dim=0)
    p.add_argument("--relations", type=_names, default=(), help="comma-separated relation labels")

    p = kind("evolving", "SBM base plus volume / bridge / novel ingest batches (A.2)",
             lambda a: evolving.generate(**_block_args(a), new_entities=a.new_entities, new_edges=a.new_edges,
                                         variants=a.variants))
    _block_model(p, embedding_dim=64)
    p.add_argument("--new-entities", type=int, default=5000)
    p.add_argument("--new-edges", type=int, default=25_000)
    p.add_argument("--variants", type=_names, default=evolving.VARIANTS)

    p = kind("embedding", "SBM with 64-dim embeddings and relation labels (A.5)",
             lambda a: sbm.generate(**_block_args(a), relations=A5_RELATIONS))
    _block_model(p, embedding_dim=64)

    p = kind("fraud", "fraud rings with hubs and flags (A.1)",
             lambda a: fraud.generate(a.honest_accounts, a.rings, hub_sizes=a.hub_sizes, seed=a.seed))
    p.add_argument("--honest-accounts", type=int, default=100_000)
    p.add_argument("--rings", type=int, default=20)
    p.add_argument("--hub-sizes", type=_ints, default=(5000, 800, 2000), help="NAT IP, device, email domain")

    p = kind("entity-resolution", "records sharing identifiers (A.3)",
             lambda a: entity_resolution.generate(a.records, a.entities, shared_fraction=a.shared_fraction,
                                                  seed=a.seed))
    p.add_argument("--records", type=int, default=200_000)
    p.add_argument("--entities", type=int, default=60_000)
    p.add_argument("--shared-fraction", type=float, default=0.02)

    p = kind("citation", "citation DAG, authorship, affiliation, planted root (A.4)",
             lambda a: citation.generate(a.papers, people=a.people, institutions=a.institutions,
                                         root_neighbourhood=a.root_neighbourhood, seed=a.seed))
    p.add_argument("--papers", type=int, default=100_000)
    p.add_argument("--people", type=int, default=30_000)
    p.add_argument("--institutions", type=int, default=2000)
    p.add_argument("--root-neighbourhood", type=int, default=400)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    args.run(args).write(args.out)


if __name__ == "__main__":
    main(sys.argv[1:])
