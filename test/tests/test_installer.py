"""Who installs pg_warren in a test database: the cluster superuser, or, for
a trusted install, a non-superuser that owns the database."""

from harness.installer import TRUSTED_ROLE, Installer


def test_by_default_the_superuser_creates_the_database_and_runs_statements():
    installer = Installer()
    assert installer.create_database_sql("test_a") == 'CREATE DATABASE "test_a"'
    assert installer.as_installer("CREATE EXTENSION pg_warren") == "CREATE EXTENSION pg_warren"


def test_a_trusted_installer_owns_the_database():
    sql = Installer.trusted().create_database_sql("test_a")
    assert sql == f'CREATE DATABASE "test_a" OWNER "{TRUSTED_ROLE}"'


def test_a_trusted_installer_runs_statements_as_its_role_and_then_resets():
    sql = Installer.trusted().as_installer("ALTER EXTENSION pg_warren UPDATE")
    assert sql == f'SET ROLE "{TRUSTED_ROLE}"; ALTER EXTENSION pg_warren UPDATE; RESET ROLE'


def test_only_a_trusted_installer_needs_a_role():
    assert Installer().create_role_sql() is None
    assert Installer.trusted().create_role_sql() == f'CREATE ROLE "{TRUSTED_ROLE}"'
