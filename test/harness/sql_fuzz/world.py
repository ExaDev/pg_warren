"""What the database holds when a call is generated: its graphs (id type,
owner, readers, flags), versions and persisted vectors, read from the
catalog between actions, so that generated calls name things that exist
as well as things that do not."""

import json
from dataclasses import dataclass, field

WORLD_SQL = """
SELECT pg_catalog.json_build_object(
  'graphs', (SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                 'name', g.name, 'id_type', g.id_type::text, 'owner', g.owner::text,
                 'readers', g.readers::text[], 'has_relations', g.has_relations, 'weighted', g.weighted)
                 ORDER BY g.name)
             FROM warren.graphs g),
  'versions', (SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                   'graph', g.name, 'id', v.version_id, 'staged', v.staged) ORDER BY v.version_id)
               FROM warren.versions v JOIN warren.graphs g USING (graph_id)),
  'vectors', (SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                  'graph', g.name, 'name', x.name, 'version', x.version_id) ORDER BY x.vector_id)
              FROM warren.vectors x JOIN warren.graphs g USING (graph_id)))
"""


@dataclass(frozen=True)
class Graph:
    name: str
    id_type: str
    owner: str
    readers: tuple = ()
    has_relations: bool = False
    weighted: bool = False

    def may_read(self, role):
        """Whether `role`, not a superuser, is the owner or a reader."""
        return role == self.owner or role in self.readers


@dataclass(frozen=True)
class World:
    graphs: tuple = ()
    versions: tuple = ()  # (graph, version id, staged)
    vectors: tuple = ()  # (graph, name, version id)
    by_name: dict = field(default_factory=dict, compare=False)

    def __post_init__(self):
        self.by_name.update({g.name: g for g in self.graphs})

    def graph(self, name):
        return self.by_name.get(name)

    def versions_of(self, name):
        return [version for graph, version, _ in self.versions if graph == name]

    def vectors_of(self, name):
        return sorted({vector for graph, vector, _ in self.vectors if graph == name})


def parse(text):
    """The world `WORLD_SQL`'s JSON describes."""
    row = json.loads(text)
    graphs = tuple(Graph(g["name"], g["id_type"], g["owner"], tuple(g["readers"] or ()), g["has_relations"],
                         g["weighted"]) for g in row["graphs"] or ())
    versions = tuple((v["graph"], v["id"], v["staged"]) for v in row["versions"] or ())
    vectors = tuple((x["graph"], x["name"], x["version"]) for x in row["vectors"] or ())
    return World(graphs, versions, vectors)
