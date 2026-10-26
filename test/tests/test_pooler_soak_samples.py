"""What the pooler soak reads from /proc and the server log."""

from harness.pooler_soak.samples import context_totals, dsm_segments, smaps_rollup

SMAPS = """55d0c0a00000-7ffd1e1f2000 ---p 00000000 00:00 0                          [rollup]
Rss:               25836 kB
Pss:                9415 kB
Pss_Dirty:          4200 kB
Pss_Anon:           3870 kB
Pss_File:           2300 kB
Pss_Shmem:          3245 kB
Shared_Clean:      12000 kB
Anonymous:          3900 kB
Swap:                  0 kB
"""


def test_a_backends_memory_is_read_from_smaps_rollup_in_bytes():
    assert smaps_rollup(SMAPS) == {"rss": 25836 * 1024, "pss": 9415 * 1024, "pss_anon": 3870 * 1024,
                                   "pss_shmem": 3245 * 1024, "anonymous": 3900 * 1024}


MAPS = """55d0c0a00000-55d0c0b00000 r--p 00000000 08:01 123 /usr/lib/postgresql/16/bin/postgres
7f0000000000-7f0000100000 rw-s 00000000 00:1a 77 /dev/shm/PostgreSQL.1234567
7f0000200000-7f0000204000 rw-s 00000000 00:1a 78 /dev/shm/PostgreSQL.42
7f0000300000-7f0000400000 rw-s 00000000 00:01 9 /SYSV0052e2c1 (deleted)
7f0000500000-7f0000580000 rw-s 00000000 00:1a 79 /dev/shm/PostgreSQL.1234567
"""


def test_the_dsm_segments_a_process_maps_are_named_with_the_bytes_it_maps():
    assert dsm_segments(MAPS) == {"PostgreSQL.1234567": 0x100000 + 0x80000, "PostgreSQL.42": 0x4000}


LOG = """2026-10-25 10:00:00.000 UTC [4242] 00000 LOG:  logging memory contexts of PID 4242
2026-10-25 10:00:00.001 UTC [4242] 00000 LOG:  level: 0; TopMemoryContext: 97664 total in 5 blocks
2026-10-25 10:00:00.002 UTC [4242] 00000 LOG:  Grand total: 1427232 bytes in 223 blocks
2026-10-25 10:00:01.002 UTC [4243] 00000 LOG:  Grand total: 2000 bytes in 2 blocks
2026-10-25 10:00:02.002 UTC [4242] 00000 LOG:  Grand total: 1500000 bytes in 223 blocks
"""


def test_each_backends_last_logged_memory_context_total_is_kept():
    assert context_totals(LOG) == {4242: 1500000, 4243: 2000}
