"""A research knowledge graph (Appendix A.4): papers citing earlier papers,
people authoring papers, people affiliated with institutions.

Citations follow preferential attachment on in-degree, so in-degree is
power-law and the graph is a DAG (a paper cites only older, smaller-id
papers). A planted root paper, the newest, cites papers until its 1–3 hop
citation neighbourhood holds at least `root_neighbourhood` papers.

Ids: papers `1..papers+1` (the root last), then people, then institutions.
The truth holds the root's neighbourhood with each paper's hop count
(`truth_root_neighbourhood`) and the GB institutions reached from it by
citation (1–3 hops) → authorship (active people) → affiliation
(`truth_walk_ends`): the expected result of A.4's walk."""

import numpy as np

from .fixture import Fixture, Table

COUNTRIES = ("US", "DE", "FR", "JP", "CN", "NL", "CA")


def generate(papers=100_000, references=7, people=30_000, papers_per_person=(1, 6), active=0.7,
             institutions=2000, gb_share=0.15, root_neighbourhood=400, seed=1):
    rng = np.random.default_rng(seed)
    cites = [[] for _ in range(papers + 2)]
    ends = []  # each paper once, plus once per citation it received
    for paper in range(1, papers + 1):
        wanted = min(paper - 1, 1 + rng.poisson(references - 1))
        cited = set()
        while len(cited) < wanted:
            cited.update(ends[i] for i in (rng.random(wanted - len(cited)) * len(ends)).astype(int))
        cites[paper] = sorted(cited)
        ends.extend(cites[paper])
        ends.append(paper)

    root = papers + 1
    hops = {}
    while len(hops) < root_neighbourhood:
        candidate = ends[int(rng.random() * len(ends))]
        if candidate in cites[root]:
            continue
        cites[root].append(candidate)
        hops = _neighbourhood(cites, root, 3)
    cites[root].sort()

    person_ids = np.arange(root + 1, root + people + 1)
    institution_ids = np.arange(root + people + 1, root + people + institutions + 1)
    is_active = np.zeros(people, dtype=bool)
    is_active[rng.choice(people, round(active * people), replace=False)] = True
    country = np.array(rng.choice(COUNTRIES, institutions), dtype=object)
    country[rng.choice(institutions, round(gb_share * institutions), replace=False)] = "GB"
    counts = rng.integers(papers_per_person[0], papers_per_person[1] + 1, people)
    authored = [np.sort(rng.choice(papers, c, replace=False) + 1) for c in counts]
    affiliation = institution_ids[rng.integers(0, institutions, people)]

    reached = {int(affiliation[i]) for i in range(people) if is_active[i] and any(p in hops for p in authored[i].tolist())}
    walk_ends = sorted(i for i in reached if country[i - institution_ids[0]] == "GB")

    citing = np.array([p for p in range(root + 1) for _ in cites[p]], dtype=np.int64)
    cited = np.array([c for p in range(root + 1) for c in cites[p]], dtype=np.int64)
    near = sorted(hops)
    return Fixture("citation", (
        Table.of("papers", id=("bigint", np.arange(1, root + 1))),
        Table.of("citations", citing=("bigint", citing), cited=("bigint", cited)),
        Table.of("people", id=("bigint", person_ids), active=("boolean", is_active)),
        Table.of("authorship", paper_id=("bigint", np.concatenate(authored)),
                 person_id=("bigint", np.repeat(person_ids, counts))),
        Table.of("institutions", id=("bigint", institution_ids), country=("text", country.tolist())),
        Table.of("affiliations", person_id=("bigint", person_ids), institution_id=("bigint", affiliation)),
        Table.of("truth_root_neighbourhood", paper_id=("bigint", np.array(near, dtype=np.int64)),
                 hops=("int", np.array([hops[p] for p in near]))),
        Table.of("truth_walk_ends", institution_id=("bigint", np.array(walk_ends, dtype=np.int64))),
    ), {
        "generator": "citation", "papers": root, "citations": len(citing), "people": people,
        "institutions": institutions, "root": root, "root_neighbourhood": len(hops),
        "walk_ends": len(walk_ends), "seed": seed,
    })


def _neighbourhood(cites, start, depth):
    """Each paper within `depth` citation hops of `start`, with its hop count."""
    hops, frontier = {}, [start]
    for hop in range(1, depth + 1):
        frontier = [c for p in frontier for c in cites[p] if c not in hops and c != start]
        frontier = list(dict.fromkeys(frontier))
        hops.update((c, hop) for c in frontier)
    return hops
