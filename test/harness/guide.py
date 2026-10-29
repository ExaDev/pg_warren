"""The SQL blocks of a guide that spans two servers (docs/beside-managed.md),
run as written. Each ```sql block starts with a comment that says where
it runs:

    -- On the managed primary: <what it does>
    -- On the graph server, as app: <what it does>
    -- On both servers: <what it does>

optionally naming the role it runs as (`as <role>`), or a condition
(`with <something>`) that marks a block CI cannot run. The comment's line
is the block's key: a check names the blocks it runs by it, so a block
renamed or added without a check fails. `:'name'` in a block is a psql
variable the check sets, for what differs between the guide's servers and
the check's (addresses, passwords)."""

import re
import subprocess
from dataclasses import dataclass

FENCE = re.compile(r"^```sql\n(.*?)^```$", re.MULTILINE | re.DOTALL)
WHERE = re.compile(r"^-- On (?P<where>the managed primary|the graph server|both servers)"
                   r"(?:, as (?P<user>\w+))?(?:, (?P<condition>with [^:]+))?: \S.*$")
SERVERS = {"the managed primary": ("primary",), "the graph server": ("graph",), "both servers": ("primary", "graph")}


@dataclass(frozen=True)
class Block:
    key: str
    sql: str
    servers: tuple
    user: str | None
    condition: str | None


def blocks(markdown):
    """The guide's SQL blocks in order. Raises `ValueError` on a block
    that does not say where it runs, or two blocks with one key."""
    found = []
    for match in FENCE.finditer(markdown):
        sql = match.group(1)
        key = sql.split("\n", 1)[0]
        where = WHERE.match(key)
        if where is None:
            raise ValueError(f"a block does not say where it runs: {key!r}")
        if any(block.key == key for block in found):
            raise ValueError(f"two blocks start with {key!r}")
        found.append(Block(key, sql, SERVERS[where["where"]], where["user"], where["condition"]))
    return found


def run(cluster, database, block, variables, user=None):
    """Runs `block` on `cluster`'s `database` through psql, which sets
    `variables` and stops at the first error; its unaligned output. A
    failure raises `RuntimeError` naming the block."""
    arguments = [arg for name, value in sorted(variables.items()) for arg in ("-v", f"{name}={value}")]
    connection = cluster.connection(database, user or block.user or "postgres")
    command = [cluster.client("psql"), "-X", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1", *connection, *arguments]
    done = subprocess.run(command, input=block.sql, capture_output=True, text=True)
    if done.returncode != 0:
        raise RuntimeError(f"{block.key} failed on {database}: {done.stdout}{done.stderr}")
    return done.stdout
