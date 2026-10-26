"""The runner: an engine's load, then each workload cold once and warm up to
`repeat` times (fewer once `budget` seconds are spent, never none), its
answer checked against the reference after the runs. A time counts only
when the answer agreed (a match or a documented difference): a fast wrong
answer is not a result. Times are wall clock around `run`, as the client
sees them."""

import statistics
import time

from .engine import EngineError, Timeout

AGREEING = ("match", "documented difference")


def _timed(engine, workload, clock):
    engine.reset(workload)
    started = clock()
    engine.run(workload)
    return clock() - started


def measure(engine, workload, check, repeat, budget, clock=time.perf_counter):
    """One workload on a loaded engine; `check(workload, answer)` returns a
    finding with a `status`."""
    reason = engine.unsupported(workload)
    if reason is not None:
        return {"status": "unsupported", "reason": reason, "counted": False}
    phases = engine.make_cold(workload)
    try:
        cold = _timed(engine, workload, clock)
        warm = [_timed(engine, workload, clock)]
        while len(warm) < repeat and cold + sum(warm) < budget:
            warm.append(_timed(engine, workload, clock))
    except Timeout as stopped:
        return {"status": "timeout", "reason": str(stopped), "counted": False, "cold_phases": phases}
    except EngineError as failed:
        return {"status": "error", "reason": str(failed), "counted": False, "cold_phases": phases}
    found = check(workload, engine.answer(workload))
    return {"cold_seconds": cold, "warm_seconds": warm, "warm_median_seconds": statistics.median(warm),
            "cold_phases": phases, "check": found, "counted": found["status"] in AGREEING}


def run(engine, workloads, check, repeat, budget, clock=time.perf_counter):
    """Loads the engine, then measures every workload in order."""
    loaded = {phase: {"seconds": samples, "median_seconds": statistics.median(samples)}
              for phase, samples in engine.load().items()}
    measured = {}
    for workload in workloads:
        measured[workload.name] = measure(engine, workload, check, repeat, budget, clock)
        print(f"    {engine.name} {workload.name}: {_summary(measured[workload.name])}", flush=True)
    return {"describe": engine.describe(), "load": loaded, "workloads": measured}


def _summary(measured):
    if "cold_seconds" not in measured:
        return f"{measured['status']} ({measured['reason']})"
    return (f"cold {measured['cold_seconds']:.4f} s, warm {measured['warm_median_seconds']:.4f} s, "
            f"{measured['check']['status']}")
