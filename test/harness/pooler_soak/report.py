"""The pooler soak's report: the time series of every sample, written as a
long CSV (`elapsed_s, metric, subject, value`) as the run goes, so a
long soak can be read while it runs; and `summary.json`, with the
run's settings, every judged series' slope, the probes, the clients'
outcomes and the failures. `lines` is the summary printed at the end."""

import csv
import json
from dataclasses import asdict

HEADER = ("elapsed_s", "metric", "subject", "value")


class SampleWriter:
    def __init__(self, path):
        self.path = path
        self.file = None
        self.writer = None

    def __enter__(self):
        self.file = open(self.path, "w", newline="")
        self.writer = csv.writer(self.file, lineterminator="\n")
        self.writer.writerow(HEADER)
        self.file.flush()
        return self

    def __exit__(self, *_):
        self.file.close()

    def write(self, rows):
        self.writer.writerows(rows)
        self.file.flush()


def slopes(judged):
    """Each judged series as a dict: its growth and whether it grew."""
    return [{"metric": j.metric, "subject": j.subject, **asdict(j.growth), "grew": j.growth.grew} for j in judged]


def write_summary(path, summary):
    path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")


def lines(judged, probes, tally, settled):
    """The summary printed at the end: the slopes of the pools' medians
    and of the cluster's series, the probes, the outcomes and chaos."""
    found = ["growth in the steady state (slope per hour; growth over the window; allowed):"]
    for j in judged:
        if "/" in j.subject:
            continue
        g = j.growth
        found.append(f"  {'GREW ' if g.grew else ''}{j.metric} [{j.subject}]: {g.slope_per_hour:,.1f}/h; "
                     f"{g.growth:,.1f} over {g.window_seconds:,.0f} s at {g.level:,.1f}; {g.allowed:,.1f} allowed")
    found.append("session state through each pool mode (carried to the client's next transaction; leaked to another):")
    found += [f"  {p.mode} {p.name}: carried {p.carried}, leaked {p.leaked}; {p.detail}" for p in probes]
    found.append("outcomes:")
    for mode, operation, sqlstate in sorted(tally.outcomes):
        found.append(f"  {mode} {operation} {'ok' if sqlstate == '00000' else sqlstate}: "
                     f"{tally.outcomes[(mode, operation, sqlstate)]}")
    found += [f"  {mode} chaos, {event}: {n}" for (mode, event), n in sorted(tally.chaos.items())]
    found.append(f"transient bytes the pools' backends keep once their clients have stopped: {settled}")
    return found
