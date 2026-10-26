"""PgBouncer's configuration and its admin console's answers."""

from pathlib import Path

from harness.pooler import Pooler, config, parse_show, users_file


def test_the_config_pools_one_database_in_the_mode_asked_for_on_a_socket_of_its_own():
    text = config("transaction", Path("/c/pooler-transaction"), Path("/c"), "5432", "soak", 6433, pool_size=8,
                  max_clients=200, admin="postgres")
    assert "[databases]\nsoak = host=/c port=5432 dbname=soak\n" in text
    for line in ("pool_mode = transaction", "listen_addr = ", "listen_port = 6433",
                 "unix_socket_dir = /c/pooler-transaction", "default_pool_size = 8", "max_client_conn = 200",
                 "admin_users = postgres", "auth_type = trust", "auth_file = /c/pooler-transaction/users.txt",
                 "server_reset_query = DISCARD ALL"):
        assert f"\n{line}\n" in text, line


def test_server_connections_live_as_long_as_the_soak_unless_a_client_breaks_one():
    text = config("session", Path("/p"), Path("/c"), "5432", "soak", 6432, 8, 64, "postgres")
    assert "\nserver_lifetime = 0\n" not in text, "0 would use each server connection once"
    lifetime = int(text.split("\nserver_lifetime = ")[1].split("\n")[0])
    idle = int(text.split("\nserver_idle_timeout = ")[1].split("\n")[0])
    assert lifetime >= 30 * 24 * 3600 and idle == 0


def test_every_user_is_listed_for_trust_authentication():
    assert users_file(["postgres", "soak_session"]) == '"postgres" ""\n"soak_session" ""\n'


def test_the_admin_console_answers_as_rows_named_by_its_header():
    text = "database\tuser\tcl_active\tsv_idle\npgbouncer\tpgbouncer\t1\t0\nsoak\tsoak_session\t12\t3\n"
    assert parse_show(text) == [{"database": "pgbouncer", "user": "pgbouncer", "cl_active": "1", "sv_idle": "0"},
                                {"database": "soak", "user": "soak_session", "cl_active": "12", "sv_idle": "3"}]
    assert parse_show("") == []


class FakeCluster:
    directory = Path("/c")
    runas = None

    @staticmethod
    def client(program):
        return f"/bin/{program}"


def test_sessions_connect_to_the_pooler_as_to_a_cluster():
    pooler = Pooler(FakeCluster(), "session", "soak", ["soak_session"], pool_size=8, max_clients=64, port=6440)
    assert pooler.connection("soak", "soak_session") == ("-h", "/c/pooler-session-6440", "-p", "6440", "-U",
                                                         "soak_session", "-d", "soak")
    assert pooler.client("psql") == "/bin/psql"
