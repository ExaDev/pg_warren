"""Appendix A fixtures: fraud rings (A.1), entity resolution (A.3) and the
research knowledge graph (A.4)."""

from collections import defaultdict

import numpy as np
import pytest

from fixtures import citation, entity_resolution, fraud


@pytest.fixture(scope="class")
def fixture(request):
    """The class's fixture, generated once per test class."""
    return request.cls.GENERATE(**request.cls.PARAMS)


def pairs(table, a, b):
    return list(zip(table.column(a).tolist(), table.column(b).tolist()))


def components(links):
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for a, b in links:
        parent[find(a)] = find(b)
    groups = defaultdict(set)
    for x in list(parent):
        groups[find(x)].add(x)
    return list(groups.values())


class TestFraud:
    PARAMS = dict(honest_accounts=3000, rings=6, hub_sizes=(400, 120, 200), seed=11)

    GENERATE = staticmethod(fraud.generate)

    def links(self, fixture):
        return (pairs(fixture.table("account_ip_events"), "account_id", "ip_id")
                + pairs(fixture.table("account_device_events"), "account_id", "device_id")
                + pairs(fixture.table("account_emails"), "account_id", "email_id"))

    def rings(self, fixture):
        table = fixture.table("truth_rings")
        rings = defaultdict(set)
        for ring, node, kind in zip(*(table.column(c).tolist() for c in ("ring", "node_id", "kind"))):
            rings[ring].add((node, kind))
        return rings

    def test_ids_are_unique_across_accounts_and_identifiers(self, fixture):
        ids = np.concatenate([fixture.table(t).column("id") for t in
                              ("accounts", "ip_addresses", "device_fingerprints", "email_addresses")])
        assert len(ids) == len(set(ids.tolist()))
        assert len(fixture.table("accounts")) == fixture.truth["accounts"]

    def test_rings_have_5_to_40_accounts_sharing_2_to_6_identifiers(self, fixture):
        rings = self.rings(fixture)
        assert len(rings) == 6
        for members in rings.values():
            accounts = [n for n, kind in members if kind == "account"]
            assert 5 <= len(accounts) <= 40
            assert 2 <= len(members) - len(accounts) <= 6

    def test_hubs_have_exactly_their_degree(self, fixture):
        degree = defaultdict(int)
        for _, identifier in self.links(fixture):
            degree[identifier] += 1
        hubs = fixture.truth["hubs"]
        assert [degree[hubs[k]] for k in ("ip", "device", "email")] == [400, 120, 200]

    def test_without_the_hubs_the_rings_are_exactly_the_multi_account_components(self, fixture):
        hubs = set(fixture.truth["hubs"].values())
        accounts = set(fixture.table("accounts").column("id").tolist())
        found = [c for c in components(l for l in self.links(fixture) if l[1] not in hubs)
                 if len(c & accounts) > 1]
        planted = [{n for n, _ in members} for members in self.rings(fixture).values()]
        assert sorted(map(sorted, found)) == sorted(map(sorted, planted))

    def test_honest_accounts_own_one_to_three_private_identifiers(self, fixture):
        hubs = set(fixture.truth["hubs"].values())
        ring_nodes = {n for members in self.rings(fixture).values() for n, _ in members}
        owned = defaultdict(set)
        users = defaultdict(set)
        for account, identifier in self.links(fixture):
            if identifier not in hubs and account not in ring_nodes:
                owned[account].add(identifier)
                users[identifier].add(account)
        assert len(owned) == 3000
        assert {len(s) for s in owned.values()} == {1, 2, 3}
        assert {len(s) for s in users.values()} == {1}

    def test_flags_seed_30_percent_of_ring_accounts_and_half_a_percent_of_honest_ones(self, fixture):
        accounts = fixture.table("accounts")
        flagged = accounts.column("confirmed_fraud") | (accounts.column("chargebacks") >= 2)
        ring_accounts = {n for m in self.rings(fixture).values() for n, k in m if k == "account"}
        in_ring = np.isin(accounts.column("id"), list(ring_accounts))
        assert flagged[in_ring].sum() == round(0.3 * len(ring_accounts)) == fixture.truth["flagged_ring_accounts"]
        assert flagged[~in_ring].sum() == round(0.005 * 3000) == fixture.truth["flagged_honest_accounts"]

    def test_is_deterministic_in_its_seed(self, fixture):
        again = fraud.generate(**self.PARAMS).table("account_ip_events").column("ip_id").tolist()
        other = fraud.generate(**dict(self.PARAMS, seed=12)).table("account_ip_events").column("ip_id").tolist()
        assert fixture.table("account_ip_events").column("ip_id").tolist() == again != other


class TestEntityResolution:
    PARAMS = dict(records=4000, entities=1200, seed=5)

    GENERATE = staticmethod(entity_resolution.generate)

    def entity_of(self, fixture):
        return dict(pairs(fixture.table("truth_entities"), "record_id", "entity_id"))

    def test_every_true_entity_has_one_to_five_records(self, fixture):
        entity_of = self.entity_of(fixture)
        assert sorted(entity_of) == sorted(fixture.table("records").column("id").tolist())
        sizes = np.bincount(list(entity_of.values()))
        assert len(entity_of) == 4000 and len(sizes) == 1200
        assert sizes.min() == 1 and sizes.max() == 5

    def test_identifier_values_are_unique_per_kind(self, fixture):
        identifiers = fixture.table("identifiers")
        assert len(set(pairs(identifiers, "kind", "value"))) == len(identifiers)
        assert set(identifiers.column("kind").tolist()) == set(entity_resolution.KINDS)

    def test_only_the_planted_shared_identifiers_link_unrelated_entities(self, fixture):
        entity_of = self.entity_of(fixture)
        entities = defaultdict(set)
        for record, identifier in pairs(fixture.table("record_identifiers"), "record_id", "identifier_id"):
            entities[identifier].add(entity_of[record])
        crossing = {i for i, e in entities.items() if len(e) > 1}
        planted = set(fixture.table("truth_shared_identifiers").column("identifier_id").tolist())
        assert crossing == planted
        assert len(planted) == round(0.02 * len(fixture.table("identifiers")))

    def test_identifier_coverage_per_source_lies_between_40_and_90_percent(self, fixture):
        coverage = np.array(fixture.truth["coverage"])
        assert coverage.shape == (3, len(entity_resolution.KINDS))
        assert ((coverage >= 0.4) & (coverage <= 0.9)).all()
        assert set(fixture.table("records").column("source").tolist()) == {1, 2, 3}

    def test_is_deterministic_in_its_seed(self, fixture):
        again = entity_resolution.generate(**self.PARAMS)
        assert self.entity_of(again) == self.entity_of(fixture)


class TestCitation:
    PARAMS = dict(papers=5000, people=1500, institutions=100, root_neighbourhood=150, seed=8)

    GENERATE = staticmethod(citation.generate)

    def cites(self, fixture):
        out = defaultdict(list)
        for a, b in pairs(fixture.table("citations"), "citing", "cited"):
            out[a].append(b)
        return out

    def test_citations_form_a_dag_from_newer_to_older_papers(self, fixture):
        links = pairs(fixture.table("citations"), "citing", "cited")
        assert len(links) == len(set(links))
        assert all(a > b for a, b in links)
        assert fixture.truth["root"] == len(fixture.table("papers")) == 5001

    def test_in_degree_is_heavy_tailed(self, fixture):
        in_degree = np.bincount(fixture.table("citations").column("cited"))
        assert in_degree.max() > 10 * in_degree[1:].mean()

    def test_the_planted_root_reaches_about_the_requested_papers_in_three_hops(self, fixture):
        cites = self.cites(fixture)
        hops, frontier = {}, [fixture.truth["root"]]
        for hop in (1, 2, 3):
            frontier = [c for p in frontier for c in cites[p] if c not in hops]
            hops.update((c, hop) for c in frontier)
        assert dict(pairs(fixture.table("truth_root_neighbourhood"), "paper_id", "hops")) == hops
        assert 150 <= len(hops) <= 200

    def test_people_and_institutions_follow_the_requested_mix(self, fixture):
        people, institutions = fixture.table("people"), fixture.table("institutions")
        assert people.column("active").sum() == round(0.7 * 1500)
        assert (institutions.column("country") == "GB").sum() == 15
        papers_per_person = np.unique(fixture.table("authorship").column("person_id"), return_counts=True)[1]
        assert len(papers_per_person) == 1500 and set(papers_per_person.tolist()) <= set(range(1, 7))
        assert sorted(fixture.table("affiliations").column("person_id").tolist()) == sorted(people.column("id").tolist())

    def test_the_truth_holds_the_uk_institutions_the_walk_must_reach(self, fixture):
        near = set(fixture.table("truth_root_neighbourhood").column("paper_id").tolist())
        active = set(fixture.table("people").column("id")[fixture.table("people").column("active")].tolist())
        authors = {p for paper, p in pairs(fixture.table("authorship"), "paper_id", "person_id")
                   if paper in near and p in active}
        gb = set(fixture.table("institutions").column("id")[fixture.table("institutions").column("country") == "GB"].tolist())
        expected = {i for p, i in pairs(fixture.table("affiliations"), "person_id", "institution_id")
                    if p in authors and i in gb}
        assert set(fixture.table("truth_walk_ends").column("institution_id").tolist()) == expected
        assert expected

    def test_is_deterministic_in_its_seed(self, fixture):
        assert self.cites(citation.generate(**self.PARAMS)) == self.cites(fixture)
