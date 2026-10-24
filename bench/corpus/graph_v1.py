"""Keepset's graph/v1 target, `{ meta, nodes, edges }`, as `keepset fetch`
writes it, read a node and an edge at a time: a large graph's document is
gigabytes, more than `json.load` should hold. The keys come in keepset's
order (meta, nodes, edges); each node and edge is a JSON object."""

import json
from contextlib import contextmanager

CHUNK = 1 << 20
WHITESPACE = " \t\r\n"


class GraphError(ValueError):
    pass


@contextmanager
def open_graph(path, chunk=CHUNK):
    with open(path, encoding="utf-8") as stream:
        yield Graph(stream, chunk)


class Graph:
    """The document's `meta`, then `nodes()` and `edges()`, each read once,
    in that order."""

    def __init__(self, stream, chunk):
        self._reader = _Reader(stream, chunk)
        self._reader.expect("{")
        self._reader.expect('"meta"')
        self._reader.expect(":")
        self.meta = self._reader.value()
        self._nodes_read = False

    def nodes(self):
        self._reader.expect(",")
        yield from self._array("nodes")
        self._nodes_read = True

    def edges(self):
        if not self._nodes_read:
            for _ in self.nodes():
                pass
        self._reader.expect(",")
        yield from self._array("edges")
        self._reader.expect("}")

    def _array(self, key):
        self._reader.expect(f'"{key}"')
        self._reader.expect(":")
        self._reader.expect("[")
        if self._reader.peek() == "]":
            self._reader.expect("]")
            return
        while True:
            yield self._reader.value()
            if self._reader.peek() == "]":
                self._reader.expect("]")
                return
            self._reader.expect(",")


class _Reader:
    def __init__(self, stream, chunk):
        self._stream, self._chunk = stream, chunk
        self._buffer, self._at, self._ended = "", 0, False
        self._decoder = json.JSONDecoder()

    def _more(self):
        if self._ended:
            return False
        text = self._stream.read(self._chunk)
        if not text:
            self._ended = True
            return False
        self._buffer = self._buffer[self._at:] + text
        self._at = 0
        return True

    def peek(self):
        while True:
            while self._at < len(self._buffer) and self._buffer[self._at] in WHITESPACE:
                self._at += 1
            if self._at < len(self._buffer):
                return self._buffer[self._at]
            if not self._more():
                return ""

    def expect(self, token):
        self.peek()
        while len(self._buffer) - self._at < len(token) and self._more():
            pass
        if not self._buffer.startswith(token, self._at):
            raise GraphError(f"expected {token} at {self._buffer[self._at:self._at + 40]!r}")
        self._at += len(token)

    def value(self):
        self.peek()
        while True:
            try:
                value, end = self._decoder.raw_decode(self._buffer, self._at)
            except json.JSONDecodeError:
                if self._more():
                    continue
                raise
            # A number at the buffer's end may continue in the next chunk.
            if end == len(self._buffer) and self._more():
                continue
            self._at = end
            return value
