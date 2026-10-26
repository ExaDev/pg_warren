"""The soak behind a connection pooler (DESIGN.md §13, M11 "Robustness";
test/pooler_soak.py): the soak's database (`fixture`), its clients and
chaos (`workload`), what each pool mode keeps of a client's session
state (`probes`), what it samples (`samples`), how growth is measured
(`growth`) and judged (`verdict`), what must hold once it stops
(`settled`), and its report (`report`)."""
