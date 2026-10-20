"""Who installs pg_warren in a test database (DESIGN.md §7, §13 M7).

By default the cluster superuser runs `CREATE EXTENSION`. A trusted install
is run by `TRUSTED_ROLE`, a non-superuser that owns the database, as on
hosted Postgres: Postgres runs the script as the bootstrap superuser, which
then owns the `warren.*` tables, while the installer owns the extension.
Everything else in a test still runs as the superuser."""

from dataclasses import dataclass

TRUSTED_ROLE = "warren_installer"


@dataclass(frozen=True)
class Installer:
    role: str | None = None  # None: the cluster superuser

    @classmethod
    def trusted(cls):
        return cls(TRUSTED_ROLE)

    def create_role_sql(self):
        """The role to create once per cluster, or None."""
        return f'CREATE ROLE "{self.role}"' if self.role else None

    def create_database_sql(self, name):
        owner = f' OWNER "{self.role}"' if self.role else ""
        return f'CREATE DATABASE "{name}"{owner}'

    def as_installer(self, statement):
        """`statement` run as the installer."""
        return f'SET ROLE "{self.role}"; {statement}; RESET ROLE' if self.role else statement

    def prepare(self, cluster):
        """Creates the installer's role in `cluster`, if it needs one."""
        sql = self.create_role_sql()
        if sql:
            cluster.database().execute(sql)
