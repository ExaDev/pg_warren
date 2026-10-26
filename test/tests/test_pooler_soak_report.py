"""The pooler soak's time series and summary."""

import csv
import json

from harness.pooler_soak.growth import Growth
from harness.pooler_soak.report import SampleWriter, slopes, write_summary
from harness.pooler_soak.verdict import Judged


def test_samples_are_written_as_a_long_csv_as_they_come(tmp_path):
    path = tmp_path / "samples.csv"
    with SampleWriter(path) as writer:
        writer.write([(5.0, "anonymous", "session/12", 1024.0)])
        assert path.read_text().splitlines() == ["elapsed_s,metric,subject,value", "5.0,anonymous,session/12,1024.0"]
        writer.write([(10.0, "segments", "cluster", 40.0)])
    assert list(csv.reader(path.open()))[-1] == ["10.0", "segments", "cluster", "40.0"]


def test_the_summary_records_every_slope_and_the_verdict(tmp_path):
    judged = [Judged("anonymous", "session/12", Growth(30, 300.0, 2e7, 1.2e5, 1e4, 5e6))]
    assert slopes(judged) == [{"metric": "anonymous", "subject": "session/12", "points": 30, "window_seconds": 300.0,
                               "level": 2e7, "slope_per_hour": 1.2e5, "growth": 1e4, "allowed": 5e6,
                               "halves": (), "grew": False}]
    path = tmp_path / "summary.json"
    write_summary(path, {"failures": ["x"], "slopes": slopes(judged)})
    assert json.loads(path.read_text())["failures"] == ["x"]
