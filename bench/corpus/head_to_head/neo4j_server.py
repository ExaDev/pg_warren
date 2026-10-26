"""One Neo4j server for the Cypher and GDS engines: Neo4j Community and the
Graph Data Science plugin, downloaded once into the cache and checked
against `pins.json`; a configuration written from the shipped one with the
head-to-head's settings; `neo4j-admin database import full` of the suite's
own tables (header files give them their shape); and the server run in
the foreground (`neo4j console`), so a restart is a stop and a start that
waits for Bolt.

Nothing of it enters git: the downloads, the installation and the
databases live under the cache (`<cache>/head-to-head`), and `close`
deletes the databases."""

import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import time
import urllib.request
from pathlib import Path

import neo4j

from .engine import EngineError, Timeout

PINS = Path(__file__).resolve().parent / "pins.json"
BOLT_PORT = 7687
START_TIMEOUT = 300
STOP_TIMEOUT = 120
NODE_LABEL = "Node"
RELATIONSHIP_TYPE = "LINK"  # the type of every edge of a dataset without relations
TIMED_OUT = ("Neo.ClientError.Transaction.TransactionTimedOut",
             "Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration")


class PinMismatch(ValueError):
    pass


def verify(path, pin):
    digest = hashlib.sha256(Path(path).read_bytes()).hexdigest()
    size = Path(path).stat().st_size
    if (digest, size) != (pin["sha256"], pin["bytes"]):
        raise PinMismatch(f"{path}: sha256 {digest}, {size} bytes; pinned {pin['sha256']}, {pin['bytes']} bytes")


def fetched(pin, downloads):
    """The pinned file, downloaded into `downloads` unless it is there; checked either way."""
    path = Path(downloads) / pin["url"].rsplit("/", 1)[1]
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        partial = path.with_suffix(path.suffix + ".partial")
        with urllib.request.urlopen(pin["url"]) as response, open(partial, "wb") as out:
            shutil.copyfileobj(response, out)
        partial.rename(path)
    verify(path, pin)
    return path


def install(cache, pins=None):
    """Neo4j's home with the GDS plugin, under `cache`; returns its path."""
    pins = pins or json.loads(PINS.read_text())
    cache = Path(cache)
    home = cache / f"neo4j-community-{pins['neo4j']['version']}"
    if not home.exists():
        with tarfile.open(fetched(pins["neo4j"], cache / "downloads")) as archive:
            archive.extractall(cache, filter="tar")
    plugin = home / "plugins" / pins["gds"]["url"].rsplit("/", 1)[1]
    if not plugin.exists():
        shutil.copy(fetched(pins["gds"], cache / "downloads"), plugin)
    verify(plugin, pins["gds"])
    return home


def configuration(shipped, settings):
    """The shipped neo4j.conf without the lines that set a key of
    `settings`, then `settings`."""
    kept = [line for line in shipped.splitlines() if line.split("=", 1)[0].strip() not in settings]
    return "\n".join(kept + [f"{key}={value}" for key, value in settings.items()]) + "\n"


def headers(dataset):
    """The header lines of the nodes and edges tables, as `tables` writes them."""
    edges = ":START_ID,:END_ID" + (",:TYPE" if dataset.relations else "") + (",weight:double" if dataset.weighted
                                                                              else "")
    return "id:ID", edges


def import_arguments(dataset, directory, header_directory):
    directory, header_directory = Path(directory), Path(header_directory)
    typed = "" if dataset.relations else f"{RELATIONSHIP_TYPE}="
    return ["database", "import", "full", "neo4j", "--overwrite-destination",
            f"--id-type={'integer' if dataset.ids == 'integer' else 'string'}",
            f"--nodes={NODE_LABEL}={header_directory}/nodes.header,{directory}/nodes.csv",
            f"--relationships={typed}{header_directory}/edges.header,{directory}/edges.csv"]


def parameters(workload, dataset):
    """A workload's start and pairs as Cypher parameters, typed as the ids were imported."""
    def typed(node):
        return int(node) if dataset.ids == "integer" else node
    return {"source": None if workload.source is None else typed(workload.source),
            "pairs": [[typed(a), typed(b)] for a, b in workload.pairs]}


class Server:
    """A Neo4j server whose state (configuration, data, logs) lives in `state`."""

    def __init__(self, home, state, heap, pagecache):
        self.home, self.state = Path(home), Path(state)
        self.heap, self.pagecache = heap, pagecache
        self.process = self.driver = None
        self.loaded = None  # (the dataset's name, what its load took)

    def settings(self):
        return {
            "server.directories.data": str(self.state / "data"),
            "server.directories.logs": str(self.state / "logs"),
            "server.directories.run": str(self.state / "run"),
            "server.memory.heap.initial_size": self.heap, "server.memory.heap.max_size": self.heap,
            "server.memory.pagecache.size": self.pagecache,
            "dbms.security.auth_enabled": "false", "dbms.security.procedures.unrestricted": "gds.*",
            "server.http.enabled": "false", "server.https.enabled": "false", "server.bolt.enabled": "true",
            "server.default_listen_address": "127.0.0.1",
            "server.bolt.listen_address": f"127.0.0.1:{BOLT_PORT}",
        }

    def _environment(self):
        return {**os.environ, "NEO4J_CONF": str(self.state / "conf")}

    def configure(self):
        (self.state / "conf").mkdir(parents=True, exist_ok=True)
        shipped = (self.home / "conf" / "neo4j.conf").read_text()
        (self.state / "conf" / "neo4j.conf").write_text(configuration(shipped, self.settings()))

    def import_dataset(self, dataset, directory):
        """Imports the dataset's tables offline (the server stopped);
        returns the seconds it took."""
        self.stop()
        header_directory = self.state / "headers"
        header_directory.mkdir(parents=True, exist_ok=True)
        for name, line in zip(("nodes", "edges"), headers(dataset)):
            (header_directory / f"{name}.header").write_text(line + "\n")
        started = time.perf_counter()
        done = subprocess.run([str(self.home / "bin" / "neo4j-admin"),
                               *import_arguments(dataset, directory, header_directory)],
                              env=self._environment(), capture_output=True, text=True)
        seconds = time.perf_counter() - started
        if done.returncode != 0:
            raise EngineError(f"neo4j-admin import failed: {done.stdout[-2000:]}{done.stderr[-2000:]}")
        return seconds

    def load(self, dataset, directory):
        """Imports the dataset, starts the server and indexes the ids, once
        for every engine on this server; returns the phases it took."""
        if self.loaded is None or self.loaded[0] != dataset.name:
            imported = self.import_dataset(dataset, directory)
            started = self.start()
            before = time.perf_counter()
            self.query(f"CREATE INDEX node_id IF NOT EXISTS FOR (n:{NODE_LABEL}) ON (n.id)")
            self.query("CALL db.awaitIndexes(3600)")
            self.loaded = (dataset.name, {"import": [imported], "start": [started],
                                          "index": [time.perf_counter() - before]})
        return dict(self.loaded[1])

    def start(self):
        """Starts the server and waits for Bolt; returns the seconds it took."""
        started = time.perf_counter()
        log = open(self.state / "console.log", "a")
        self.process = subprocess.Popen([str(self.home / "bin" / "neo4j"), "console"], env=self._environment(),
                                        stdout=log, stderr=subprocess.STDOUT)
        self.driver = neo4j.GraphDatabase.driver(f"bolt://127.0.0.1:{BOLT_PORT}", auth=None)
        while True:
            try:
                self.driver.verify_connectivity()
                return time.perf_counter() - started
            except neo4j.exceptions.ServiceUnavailable:
                if self.process.poll() is not None or time.perf_counter() - started > START_TIMEOUT:
                    raise EngineError(f"neo4j did not start; see {self.state / 'console.log'}") from None
                time.sleep(0.1)

    def stop(self):
        if self.driver is not None:
            self.driver.close()
            self.driver = None
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(STOP_TIMEOUT)
            except subprocess.TimeoutExpired:  # a shutdown waits for running transactions
                self.process.kill()
                self.process.wait()
            self.process = None

    def restart(self):
        self.stop()
        return self.start()

    def query(self, cypher, parameters=None, timeout=None):
        """The records of `cypher` as dicts; a transaction timeout raises `Timeout`."""
        try:
            with self.driver.session() as session:
                return session.run(neo4j.Query(cypher, timeout=timeout), parameters or {}).data()
        except neo4j.exceptions.ClientError as error:
            if error.code in TIMED_OUT:
                raise Timeout(f"{timeout} s") from error
            raise EngineError(f"{error.code}: {error.message}") from error
        except neo4j.exceptions.Neo4jError as error:
            raise EngineError(f"{error.code}: {error.message}") from error
        except neo4j.exceptions.DriverError as error:  # the connection lost, the server gone
            raise EngineError(f"{type(error).__name__}: {error}") from error

    def store_bytes(self):
        databases = self.state / "data" / "databases" / "neo4j"
        return sum(p.stat().st_size for p in databases.rglob("*") if p.is_file())

    def versions(self):
        components = self.query("CALL dbms.components() YIELD name, versions, edition RETURN *")
        return {"neo4j": components[0]["versions"][0], "edition": components[0]["edition"],
                "gds": self.query("RETURN gds.version() AS version")[0]["version"],
                "python_driver": neo4j.__version__, "java": _java_version()}

    def close(self):
        """Stops the server and deletes its databases."""
        self.stop()
        self.loaded = None
        shutil.rmtree(self.state / "data", ignore_errors=True)


def _java_version():
    done = subprocess.run(["java", "-version"], capture_output=True, text=True)
    return next((line for line in done.stderr.splitlines() if "version" in line), "unknown")
