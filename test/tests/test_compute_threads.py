"""The compute threads check's measurement (harness/compute_threads.py)."""

from harness.compute_threads import Window, compute_threads, compute_ticks, held_problems, stat_ticks, within_cap

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


def test_threads_shown_held_stay_within_the_cap_and_some_are_shown():
    def window(held, by_one):
        return Window(seconds=1.0, cpu_seconds=1.0, calls=1, failures=(), waits_seen=1,
                      most_held=held, most_held_by_one=by_one)

    assert held_problems(window(2, 1), 2) == []
    assert len(held_problems(window(3, 1), 2)) == 1, "over the cap in the cluster"
    assert len(held_problems(window(2, 3), 2)) == 1, "over the cap in one backend"
    assert len(held_problems(window(0, 0), 2)) == 1, "none ever shown"
