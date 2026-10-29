"""The compute threads check's measurement (harness/compute_threads.py)."""

from harness.compute_threads import Window, compute_threads, compute_ticks, stat_ticks, within_cap

# A /proc stat line whose command name holds a space and a parenthesis.
LINE = "4242 (postgres: a) b) S 1 2 3 4 5 6 7 8 9 10 {utime} {stime} 0 0 20 0 5 0 99 0 0"


def test_stat_ticks_are_user_and_system_time_past_the_command_name():
    assert stat_ticks(LINE.format(utime=150, stime=25)) == 175


def test_compute_ticks_are_the_processes_less_its_main_threads(tmp_path):
    (tmp_path / "7" / "task" / "7").mkdir(parents=True)
    (tmp_path / "7" / "stat").write_text(LINE.format(utime=900, stime=100))
    (tmp_path / "7" / "task" / "7" / "stat").write_text(LINE.format(utime=200, stime=50))
    assert compute_ticks(7, proc=tmp_path) == 750
    assert compute_ticks(8, proc=tmp_path) == 0, "a backend that has gone"


def test_compute_threads_are_counted_by_name(tmp_path):
    for task, name in [(7, "postgres"), (8, "warren-compute-"), (9, "warren-compute-"), (10, "other")]:
        (tmp_path / "7" / "task" / str(task)).mkdir(parents=True)
        (tmp_path / "7" / "task" / str(task) / "comm").write_text(f"{name}\n")
    assert compute_threads(7, proc=tmp_path) == 2
    assert compute_threads(8, proc=tmp_path) == 0


def test_a_window_is_within_the_cap_and_its_slack():
    def window(cpu):
        return Window(seconds=10.0, cpu_seconds=cpu, calls=5, failures=(), waits_seen=1)

    assert window(20.0).cores == 2.0
    assert within_cap(window(12.0), 1)
    assert not within_cap(window(13.0), 1)
    assert within_cap(window(20.0), 2, slack=0.0)
