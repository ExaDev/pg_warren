"""A scratch cluster's server log, read for trouble after a run: errors,
warnings and fatals whose SQLSTATE the run did not expect, any PANIC,
backends that crashed or exited abnormally, assertion failures (`TRAP:`)
and Rust panics. `SETTINGS` makes the log carry each line's SQLSTATE."""

import re

SETTINGS = ("log_line_prefix = '%m [%p] %e '",)

LINE = re.compile(r"\[\d+\] (?P<sqlstate>[0-9A-Z]{5}) (?P<level>[A-Z]+[0-9]?):\s")
CHECKED_LEVELS = {"WARNING", "ERROR", "FATAL"}
TROUBLE = re.compile(r"PANIC:|TRAP:|panicked at|terminated by signal|exited with exit code|"
                     r"terminated by exception|terminating any other active server processes")


def problems(text, allowed=frozenset(), expected=()):
    """The lines of server log `text` that show trouble, except those that
    match one of the regular expressions `expected`: a WARNING, ERROR or
    FATAL whose SQLSTATE is not in `allowed`, and any line of `TROUBLE`."""
    exempt = [re.compile(pattern) for pattern in expected]
    found = []
    for line in text.splitlines():
        parsed = LINE.search(line)
        unexpected_state = (parsed is not None and parsed["level"] in CHECKED_LEVELS
                            and parsed["sqlstate"] not in allowed)
        if (unexpected_state or TROUBLE.search(line)) and not any(p.search(line) for p in exempt):
            found.append(line)
    return found

# What a clean shutdown logs through PG 16: the logical replication
# launcher's exit with code 1. For runs that stop or restart a server
# before reading its log.
CLEAN_SHUTDOWN = (r'background worker "logical replication launcher" \(PID \d+\) exited with exit code 1',)
