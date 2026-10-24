"""bench/corpus/ids.py: the byte lengths of a graph's text ids, and what
front coding (DESIGN.md §5.7, deferred) would make of them."""

import pytest

from corpus import ids


def test_the_distribution_counts_utf8_bytes_not_characters():
    found = ids.distribution(["ab", "abc", "é", "abcd"])
    assert found["count"] == 4
    assert found["bytes"] == 2 + 3 + 2 + 4
    assert (found["min"], found["max"], found["mean"]) == (2, 4, 2.75)
    assert found["percentiles"] == {"50": 2, "90": 4, "99": 4}
    assert found["lengths"] == {"2": 2, "3": 1, "4": 1}


def test_front_coding_stores_a_blocks_first_id_whole_and_the_rest_as_suffixes():
    # Sorted bytewise: "Gene::1", "Gene::12", "Gene::2" in one block: the
    # first whole (length 1 byte + 7), then (shared prefix, suffix length,
    # suffix): (7, 1, "2") and (6, 1, "2"), a byte for each number.
    coded = ids.front_coding(["Gene::2", "Gene::12", "Gene::1"], block=16)
    assert coded["bytes"] == (1 + 7) + (1 + 1 + 1) + (1 + 1 + 1)
    assert coded["blocks"] == 1
    assert coded["offsets_bytes"] == 8
    assert coded["mean_shared_prefix"] == pytest.approx((7 + 6) / 2)


def test_each_block_starts_again_from_a_whole_id():
    coded = ids.front_coding(["aa", "ab", "ac"], block=2)
    # block 1: "aa" whole (1 + 2), "ab" as (1, 1, "b"); block 2: "ac" whole.
    assert coded["bytes"] == 3 + 3 + 3
    assert coded["blocks"] == 2


def test_a_length_past_127_takes_a_second_varint_byte():
    long = "x" * 200
    assert ids.front_coding([long], block=16)["bytes"] == 2 + 200


def test_the_report_compares_the_id_bytes_with_and_without_front_coding():
    report = ids.report(["https://example.org/a", "https://example.org/b"], nodes=2)
    assert report["id_map"]["id_bytes"] == 42
    assert report["id_map"]["id_offsets"] == 8 * 3
    assert report["id_map"]["id_order"] == 4 * 2
    assert report["front_coded"]["ratio"] == pytest.approx(42 / report["front_coded"]["bytes"])
