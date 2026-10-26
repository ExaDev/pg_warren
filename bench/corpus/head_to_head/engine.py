"""The port every engine implements, and the errors it raises. An engine is
made for one dataset and holds it once loaded:

- `name`, and `describe()`: its version, threads and settings, recorded
  with the results;
- `load()`: ingests the dataset's tables and builds what its algorithms
  read, as phases of samples, `{"ingest": [seconds], "build": [...]}`;
- `unsupported(workload)`: None, or why the engine cannot run it;
- `make_cold(workload)`: puts the engine in the state a first call meets
  (evicted, restarted); returns the phases that took, `{name: seconds}`;
- `reset(workload)`: clears what a previous run left (an output vector or
  property), untimed, before every run;
- `run(workload)`: computes the answer into the engine's own memory; this
  is what is timed; it raises `Timeout` or `EngineError`;
- `answer(workload)`: reads the last run's answer out, untimed, in the
  form `answers.Reference.check` takes;
- `close()`: frees what it holds."""


class EngineError(RuntimeError):
    """A run failed."""


class Timeout(EngineError):
    """A run passed the time bound and was stopped."""
