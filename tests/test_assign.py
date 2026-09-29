"""Regression tests for resume.py pane -> session assignment.

Run from the repo root:  python3 -m unittest discover -s tests -t .
Stdlib only.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import resume


def panes(*ids, cwd="/repo"):
    return [{"pane_id": pid, "cwd": cwd} for pid in ids]


def sessions(*names):
    return [{"session_id": f"id-{n}", "session_name": n, "prompt_count": 1}
            for n in names]


class AssignSessionsTest(unittest.TestCase):
    def test_same_cwd_panes_get_distinct_sessions(self):
        # Regression: two panes sharing one cwd must resume two sessions,
        # not skip the second as "already resumed this run".
        got, skips = resume.assign_sessions(
            panes("w1:p1", "w2:p1"), lambda cwd: sessions("a", "b", "c"), {})
        self.assertEqual(skips, {})
        self.assertEqual(got["w1:p1"]["session_name"], "a")
        self.assertEqual(got["w2:p1"]["session_name"], "b")

    def test_three_panes_same_cwd(self):
        got, skips = resume.assign_sessions(
            panes("w1:p1", "w2:p1", "w3:p1"),
            lambda cwd: sessions("a", "b", "c"), {},
        )
        self.assertEqual(skips, {})
        self.assertEqual([got[f"w{i}:p1"]["session_name"] for i in (1, 2, 3)],
                         ["a", "b", "c"])

    def test_stable_mapping_keeps_own_session(self):
        mapping = {"w2:p1": "id-b"}
        got, skips = resume.assign_sessions(
            panes("w1:p1", "w2:p1"), lambda cwd: sessions("a", "b", "c"),
            mapping,
        )
        self.assertEqual(skips, {})
        self.assertEqual(got["w2:p1"]["session_name"], "b")
        self.assertEqual(got["w1:p1"]["session_name"], "a")

    def test_stale_mapping_falls_back_to_newest_free(self):
        mapping = {"w1:p1": "id-gone"}
        got, skips = resume.assign_sessions(
            panes("w1:p1"), lambda cwd: sessions("a", "b"), mapping)
        self.assertEqual(skips, {})
        self.assertEqual(got["w1:p1"]["session_name"], "a")

    def test_more_panes_than_sessions(self):
        got, skips = resume.assign_sessions(
            panes("w1:p1", "w2:p1", "w3:p1"),
            lambda cwd: sessions("a", "b"), {},
        )
        self.assertEqual(set(got), {"w1:p1", "w2:p1"})
        self.assertIn("w3:p1", skips)

    def test_no_history_everywhere(self):
        got, skips = resume.assign_sessions(panes("w1:p1", "w2:p1"),
                                            lambda cwd: [], {})
        self.assertEqual(got, {})
        self.assertEqual(set(skips), {"w1:p1", "w2:p1"})

    def test_different_cwds_are_independent(self):
        ps = [{"pane_id": "w1:p1", "cwd": "/a"},
              {"pane_id": "w2:p1", "cwd": "/b"}]
        got, skips = resume.assign_sessions(
            ps, lambda cwd: sessions("x") if cwd == "/a" else sessions("y"),
            {},
        )
        self.assertEqual(skips, {})
        self.assertEqual(got["w1:p1"]["session_name"], "x")
        self.assertEqual(got["w2:p1"]["session_name"], "y")

    def test_symlinked_cwds_share_one_pool(self):
        # /tmp/link -> /tmp/real: panes in both are one cwd group.
        with tempfile.TemporaryDirectory() as tmp:
            real = os.path.join(tmp, "real")
            os.mkdir(real)
            link = os.path.join(tmp, "link")
            os.symlink(real, link)
            ps = [{"pane_id": "w1:p1", "cwd": real},
                  {"pane_id": "w2:p1", "cwd": link}]
            got, skips = resume.assign_sessions(
                ps, lambda cwd: sessions("a", "b"), {})
        self.assertEqual(skips, {})
        self.assertEqual({got["w1:p1"]["session_name"],
                          got["w2:p1"]["session_name"]}, {"a", "b"})


class EachWithPauseTest(unittest.TestCase):
    def test_pauses_between_items_only(self):
        calls = []
        got = list(resume.each_with_pause(["a", "b", "c"], 2, calls.append))
        self.assertEqual([item for _, item in got], ["a", "b", "c"])
        self.assertEqual(calls, [2, 2])

    def test_no_pause_for_single_item(self):
        calls = []
        got = list(resume.each_with_pause(["a"], 5, calls.append))
        self.assertEqual([item for _, item in got], ["a"])
        self.assertEqual(calls, [])

    def test_non_positive_stagger_disables_pausing(self):
        for stagger in (0, -1):
            calls = []
            list(resume.each_with_pause(["a", "b"], stagger, calls.append))
            self.assertEqual(calls, [])


class MappingFileTest(unittest.TestCase):
    def test_roundtrip_and_missing_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "sub", "resume-state.json")
            self.assertEqual(resume.load_mapping(path), {})
            resume.save_mapping(path, {"w1:p1": "id-a"})
            self.assertEqual(resume.load_mapping(path), {"w1:p1": "id-a"})

    def test_corrupt_file_yields_empty_mapping(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "resume-state.json")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write("{not json")
            self.assertEqual(resume.load_mapping(path), {})


if __name__ == "__main__":
    unittest.main()
