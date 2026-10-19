"""Fraud rings (Appendix A.1): accounts linked to the IP addresses, device
fingerprints and email addresses they use.

- Honest accounts each own 1–3 private identifiers.
- Each planted ring has 5–40 accounts sharing 2–6 identifiers: every member
  uses the ring's first identifier and at least one other, and every ring
  identifier has at least two users.
- Three hub identifiers (a NAT IP, an office device, a disposable-email
  domain) are used by uniformly drawn accounts, honest and ring alike.
- 30% of ring accounts and 0.5% of honest accounts are flagged
  (`confirmed_fraud OR chargebacks >= 2`).

Account ids are shuffled so that ring membership is not visible in them;
identifier ids follow the accounts'. The truth holds every ring's accounts
and identifiers (`truth_rings`) and the hubs' ids."""

import numpy as np

from .fixture import Fixture, Table

KINDS = ("ip", "device", "email")
KIND_TABLES = {"ip": "ip_addresses", "device": "device_fingerprints", "email": "email_addresses"}
EVENT_TABLES = {"ip": ("account_ip_events", "ip_id"), "device": ("account_device_events", "device_id"),
                "email": ("account_emails", "email_id")}


def generate(honest_accounts=100_000, rings=20, ring_size=(5, 40), ring_identifiers=(2, 6),
             hub_sizes=(5000, 800, 2000), ring_flagged=0.3, honest_flagged=0.005, seed=1):
    rng = np.random.default_rng(seed)
    sizes = rng.integers(ring_size[0], ring_size[1] + 1, rings)
    accounts = honest_accounts + int(sizes.sum())
    account_ids = rng.permutation(accounts) + 1
    honest, ring_accounts = account_ids[:honest_accounts], np.split(account_ids[honest_accounts:], np.cumsum(sizes)[:-1])

    identifiers = _Identifiers(accounts + 1)
    for account in honest:
        for kind in rng.choice(KINDS, rng.integers(1, 4)):
            identifiers.link(account, identifiers.new(kind))

    truth_rings = []
    for ring, members in enumerate(ring_accounts):
        shared = [identifiers.new(kind) for kind in rng.choice(KINDS, rng.integers(ring_identifiers[0], ring_identifiers[1] + 1))]
        users = {i: set() for i in shared}
        for account in members:
            others = rng.choice(shared[1:], rng.integers(1, len(shared)), replace=False)
            for identifier in [shared[0], *others]:
                users[identifier].add(int(account))
        for identifier, accounts_using in users.items():
            while len(accounts_using) < 2:
                accounts_using.add(int(rng.choice(members)))
            for account in sorted(accounts_using):
                identifiers.link(account, identifier)
        truth_rings += [(ring, int(a), "account") for a in members]
        truth_rings += [(ring, i, identifiers.kind[i]) for i in shared]

    hubs = {}
    for kind, size in zip(KINDS, hub_sizes):
        hubs[kind] = identifiers.new(kind)
        for account in np.sort(rng.choice(account_ids, size, replace=False)):
            identifiers.link(int(account), hubs[kind])

    flagged = np.zeros(accounts + 1, dtype=bool)
    in_rings = np.concatenate(ring_accounts)
    flagged_ring, flagged_honest = round(ring_flagged * len(in_rings)), round(honest_flagged * honest_accounts)
    flagged[rng.choice(in_rings, flagged_ring, replace=False)] = True
    flagged[rng.choice(honest, flagged_honest, replace=False)] = True
    ids = np.arange(1, accounts + 1)
    confirmed = flagged[ids] & (rng.random(accounts) < 0.5)
    chargebacks = np.where(flagged[ids] & ~confirmed, rng.integers(2, 5, accounts),
                           np.where(flagged[ids], rng.integers(0, 5, accounts), rng.integers(0, 2, accounts)))

    tables = [Table.of("accounts", id=("bigint", ids), confirmed_fraud=("boolean", confirmed),
                       chargebacks=("int", chargebacks))]
    tables += [Table.of(KIND_TABLES[k], id=("bigint", np.array(identifiers.of_kind(k), dtype=np.int64))) for k in KINDS]
    for kind in KINDS:
        name, column = EVENT_TABLES[kind]
        links = [(a, i) for a, i in identifiers.links if identifiers.kind[i] == kind]
        tables.append(Table.of(name, account_id=("bigint", np.array([a for a, _ in links], dtype=np.int64)),
                               **{column: ("bigint", np.array([i for _, i in links], dtype=np.int64))}))
    ring_col, node_col, kind_col = zip(*truth_rings)
    tables.append(Table.of("truth_rings", ring=("int", np.array(ring_col)), node_id=("bigint", np.array(node_col)),
                           kind=("text", list(kind_col))))
    return Fixture("fraud", tuple(tables), {
        "generator": "fraud", "accounts": accounts, "honest_accounts": honest_accounts,
        "ring_accounts": len(in_rings), "rings": rings, "hubs": hubs,
        "hub_sizes": dict(zip(KINDS, hub_sizes)), "flagged_ring_accounts": flagged_ring,
        "flagged_honest_accounts": flagged_honest, "seed": seed,
    })


class _Identifiers:
    """Allocates identifier ids from `first` and records account links."""

    def __init__(self, first):
        self.next = first
        self.kind = {}
        self.links = []

    def new(self, kind):
        identifier, self.next = self.next, self.next + 1
        self.kind[identifier] = str(kind)
        return identifier

    def link(self, account, identifier):
        self.links.append((int(account), int(identifier)))

    def of_kind(self, kind):
        return [i for i, k in self.kind.items() if k == kind]
