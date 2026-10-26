"""The real-graph suite (DESIGN.md §12, §13 M11): datasets from the keepset
catalogue, fetched by its recipes and pinned by checksum (`manifest`,
`keepset`, `fetch`), loaded into the `nodes` and `edges` tables
`warren.create_graph` reads (`graph_v1`, `tables`), checked against
NetworkX (`oracle`, `checks`), timed on the §12 matrix (`performance`),
timed against igraph and Neo4j (`head_to_head`), and their text ids
measured for front coding (`ids`). `suite.py` is the
command line."""
