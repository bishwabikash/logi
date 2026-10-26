"""Offline tests for g502.appwatch — synthetic process lists, no device.

Nothing here reads the real /proc or touches hardware: process snapshots are
plain lists of Process, and the /proc readers are pointed at a temp tree.
Run with: python3 -m unittest discover -s tests
"""
from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from g502.appwatch import (  # noqa: E402
    Action, DeviceApplier, ForegroundProbe, Process, Rule, RuleSet, Watcher,
    _first_json, _ProbeUnavailable, _Strategy, read_process,
    snapshot_processes,
)


def proc(comm: str, exe: str = "", cmdline: str = "", pid: int = 1) -> Process:
    return Process(pid=pid, comm=comm, exe=exe or comm,
                   cmdline=cmdline or comm)


class Clock:
    """Monotonic clock we control, so debounce tests take no wall time."""

    def __init__(self, t: float = 1000.0):
        self.t = t

    def __call__(self) -> float:
        return self.t

    def advance(self, dt: float) -> None:
        self.t += dt


class TestRuleMatching(unittest.TestCase):
    def test_match_hits_comm_exe_and_cmdline(self):
        r = Rule(match=("blender",), profile=2)
        self.assertTrue(r.matches(proc("blender")))
        self.assertTrue(r.matches(proc("wrapper", exe="blender",
                                       cmdline="/usr/bin/wrapper")))
        self.assertTrue(r.matches(proc("sh", exe="sh",
                                       cmdline="sh -c blender --factory")))
        self.assertFalse(r.matches(proc("gimp")))

    def test_comm_is_exact_not_substring(self):
        r = Rule(comm=("cs2",), profile=1)
        self.assertTrue(r.matches(proc("cs2")))
        self.assertFalse(r.matches(proc("cs2_launcher")))
        # ...and a cmdline mention alone is not enough for a comm rule
        self.assertFalse(r.matches(proc("bash", cmdline="bash ./cs2.sh")))

    def test_comm_rule_tolerates_kernel_truncation(self):
        # /proc/<pid>/comm is cut to 15 chars, so the full name never matches
        # literally; the rule must still fire.
        r = Rule(comm=("chrome_crashpad_handler",), profile=3)
        self.assertTrue(r.matches(Process(pid=7, comm="chrome_crashpad")))

    def test_cmdline_is_substring(self):
        r = Rule(cmdline=("--app-id=com.example",), profile=1)
        self.assertTrue(r.matches(
            proc("electron", cmdline="electron --app-id=com.example.app")))
        self.assertFalse(r.matches(proc("electron", cmdline="electron other")))

    def test_rule_without_patterns_matches_nothing(self):
        r = Rule(profile=4)
        self.assertFalse(r.matches(proc("anything")))

    def test_disabled_rule_never_matches(self):
        r = Rule(match=("blender",), profile=2, enabled=False)
        self.assertFalse(r.matches(proc("blender")))

    def test_patterns_are_case_insensitive(self):
        r = Rule.from_dict({"match": ["Blender"], "profile": 2})
        self.assertTrue(r.matches(proc("blender")))

    def test_label_prefers_name_then_first_pattern(self):
        self.assertEqual(Rule(match=("cs2",), name="shooter").label, "shooter")
        self.assertEqual(Rule(match=("cs2",)).label, "cs2")
        self.assertEqual(Rule().label, "rule")


class TestRuleSetPrecedence(unittest.TestCase):
    def setUp(self):
        self.rs = RuleSet(
            rules=[Rule(match=("cs2",), profile=1, dpi=800),
                   Rule(match=("blender",), profile=2),
                   Rule(match=("steam",), profile=3)],
            default_profile=0, default_dpi=1600)

    def test_first_matching_rule_wins(self):
        # Both blender and steam are up; the earlier rule decides.
        a = self.rs.match([proc("steam"), proc("blender")])
        self.assertEqual(a.profile, 2)
        self.assertEqual(a.source, "process")

    def test_rule_order_not_process_order(self):
        a = self.rs.match([proc("steam"), proc("cs2"), proc("blender")])
        self.assertEqual((a.profile, a.dpi), (1, 800))

    def test_default_applies_when_nothing_matches(self):
        a = self.rs.match([proc("gnome-shell"), proc("systemd")])
        self.assertEqual((a.profile, a.dpi, a.source), (0, 1600, "default"))

    def test_unset_dpi_falls_back_to_default(self):
        # Leaving blender's dpi unset must return to the default, not keep
        # whatever the previous app asked for.
        self.assertEqual(self.rs.match([proc("blender")]).dpi, 1600)

    def test_effect_inheritance(self):
        rs = RuleSet(rules=[Rule(match=("a",), profile=1, effect="rainbow"),
                            Rule(match=("b",), profile=2)],
                     default_effect="solid")
        self.assertEqual(rs.match([proc("a")]).effect, "rainbow")
        self.assertEqual(rs.match([proc("b")]).effect, "solid")

    def test_empty_ruleset_yields_default(self):
        a = RuleSet(default_profile=4).match([proc("cs2")])
        self.assertEqual((a.profile, a.rule, a.source), (4, "", "default"))


class TestForegroundPrecedence(unittest.TestCase):
    def setUp(self):
        self.rs = RuleSet(rules=[Rule(match=("launcher",), profile=1),
                                 Rule(match=("cs2",), profile=2)],
                          default_profile=0)

    def test_foreground_beats_earlier_rule_on_presence(self):
        a = self.rs.match([proc("launcher"), proc("cs2")],
                          foreground=proc("cs2"))
        self.assertEqual((a.profile, a.source), (2, "foreground"))

    def test_unmatched_foreground_falls_back_to_presence(self):
        a = self.rs.match([proc("launcher"), proc("cs2")],
                          foreground=proc("nautilus"))
        self.assertEqual((a.profile, a.source), (1, "process"))

    def test_no_foreground_info_uses_presence(self):
        a = self.rs.match([proc("cs2")], foreground=None)
        self.assertEqual((a.profile, a.source), (2, "process"))

    def test_synthetic_wm_class_matches_rules(self):
        # The probe often has only a WM_CLASS; it must still match by name.
        a = self.rs.match([], foreground=Process.synthetic("CS2"))
        self.assertEqual((a.profile, a.source), (2, "foreground"))


class TestSerialisation(unittest.TestCase):
    def test_round_trip_through_dicts(self):
        rs = RuleSet(
            rules=[Rule(match=("cs2", "csgo_linux64"), profile=1, dpi=800,
                        effect="solid", name="shooter"),
                   Rule(comm=("blender",), cmdline=("--factory",), profile=2,
                        enabled=False)],
            default_profile=3, default_dpi=1600, default_effect="rainbow")
        again = RuleSet.from_config(rs.to_config())
        self.assertEqual(again, rs)

    def test_reads_the_shipped_config_shape(self):
        # This is config.DEFAULT_CONFIG's shape: "match" as a list, a loose
        # "comment" label, optional dpi.
        cfg = {"default_profile": 0, "restore_on_exit": True,
               "rules": [{"match": ["csgo_linux64", "cs2"], "profile": 1,
                          "comment": "a game by process name"},
                         {"match": "blender", "profile": 2, "dpi": 1200}]}
        rs = RuleSet.from_config(cfg)
        self.assertEqual(len(rs.rules), 2)
        self.assertEqual(rs.rules[0].name, "a game by process name")
        self.assertEqual(rs.rules[1].match, ("blender",))  # bare str accepted
        self.assertEqual(rs.match([proc("cs2")]).profile, 1)

    def test_to_dict_omits_defaults(self):
        self.assertEqual(Rule(match=("cs2",), profile=1).to_dict(),
                         {"match": ["cs2"], "profile": 1})

    def test_rejects_out_of_range_profile(self):
        with self.assertRaises(ValueError):
            Rule.from_dict({"match": "x", "profile": 5})
        with self.assertRaises(ValueError):
            Rule.from_dict({"match": "x", "profile": -1})

    def test_rejects_wrong_types(self):
        for bad in ({"match": 5, "profile": 0},
                    {"match": "x", "profile": "1"},
                    {"match": "x", "dpi": "800"},
                    {"match": "x", "effect": 7}):
            with self.assertRaises(ValueError):
                Rule.from_dict(bad)
        with self.assertRaises(ValueError):
            RuleSet.from_config({"rules": "nope"})
        with self.assertRaises(ValueError):
            RuleSet.from_config({"default_profile": 9})


class TestAction(unittest.TestCase):
    def test_equality_ignores_diagnostics(self):
        # Two different rules asking for the same device state must not
        # trigger a re-apply.
        self.assertEqual(Action(1, 800, rule="a", source="process"),
                         Action(1, 800, rule="b", source="foreground"))
        self.assertNotEqual(Action(1, 800), Action(1, 1600))


class TestWatcher(unittest.TestCase):
    def build(self, rules, procs, **kw):
        self.applied: list[Action] = []
        self.procs = list(procs)
        self.clock = Clock()
        w = Watcher(self.applied.append, rules,
                    snapshot=lambda: list(self.procs),
                    foreground=lambda: None, clock=self.clock,
                    sleep=lambda _s: None, **kw)
        return w

    def test_applies_after_debounce_only(self):
        w = self.build([{"match": "cs2", "profile": 1}], [proc("cs2")],
                       debounce=2.0)
        self.assertIsNone(w.tick())        # first sighting: candidate only
        self.assertEqual(self.applied, [])
        self.clock.advance(1.0)
        self.assertIsNone(w.tick())        # still inside the window
        self.clock.advance(1.5)
        self.assertEqual(w.tick().profile, 1)
        self.assertEqual([a.profile for a in self.applied], [1])

    def test_blip_does_not_switch(self):
        w = self.build([{"match": "installer", "profile": 1}], [],
                       debounce=2.0)
        self.procs = [proc("installer")]
        w.tick()
        self.clock.advance(0.5)
        self.procs = []                    # gone again before the window ends
        w.tick()
        self.clock.advance(5.0)
        w.tick()
        # The blip's profile was never written; only the fallback settled.
        self.assertEqual([a.profile for a in self.applied], [0])
        self.assertEqual(self.applied[0].source, "default")

    def test_idempotent_while_state_holds(self):
        w = self.build([{"match": "cs2", "profile": 1}], [proc("cs2")],
                       debounce=0.0)
        for _ in range(5):
            w.tick()
            self.clock.advance(1.0)
        self.assertEqual(len(self.applied), 1)

    def test_switch_back_to_default_on_exit_of_app(self):
        w = self.build([{"match": "cs2", "profile": 1}], [proc("cs2")],
                       debounce=0.0)
        w.tick()
        self.procs = []
        self.clock.advance(1.0)
        w.tick()
        self.assertEqual([a.profile for a in self.applied], [1, 0])
        self.assertEqual(self.applied[-1].source, "default")

    def test_failed_apply_is_retried(self):
        calls: list[Action] = []

        def flaky(action: Action) -> None:
            calls.append(action)
            if len(calls) == 1:
                raise RuntimeError("device busy")

        clock = Clock()
        w = Watcher(flaky, [{"match": "cs2", "profile": 1}],
                    snapshot=lambda: [proc("cs2")], foreground=lambda: None,
                    clock=clock, debounce=1.0, sleep=lambda _s: None)
        w.tick()
        clock.advance(2.0)
        self.assertIsNone(w.tick())        # first attempt raises
        self.assertIsNone(w.applied)
        clock.advance(2.0)
        self.assertEqual(w.tick().profile, 1)
        self.assertEqual(len(calls), 2)

    def test_foreground_callable_is_used(self):
        clock = Clock()
        applied: list[Action] = []
        w = Watcher(applied.append,
                    [{"match": "launcher", "profile": 1},
                     {"match": "cs2", "profile": 2}],
                    snapshot=lambda: [proc("launcher"), proc("cs2")],
                    foreground=lambda: proc("cs2"), clock=clock,
                    debounce=0.0, sleep=lambda _s: None)
        self.assertEqual(w.tick().profile, 2)

    def test_run_stops_after_ticks_and_calls_exit_hook(self):
        done: list[str] = []
        w = self.build([{"match": "cs2", "profile": 1}], [proc("cs2")],
                       debounce=0.0, on_exit=lambda: done.append("restored"))
        w.run(ticks=3)
        self.assertEqual(done, ["restored"])
        self.assertEqual(len(self.applied), 1)

    def test_run_honours_stop_predicate(self):
        w = self.build([{"match": "cs2", "profile": 1}], [proc("cs2")],
                       debounce=0.0)
        ticks = {"n": 0}

        def stop() -> bool:
            ticks["n"] += 1
            return ticks["n"] > 2
        w.run(stop=stop)
        self.assertEqual(len(self.applied), 1)

    def test_exit_hook_failure_is_swallowed(self):
        def boom() -> None:
            raise RuntimeError("mouse unplugged")
        w = self.build([], [], debounce=0.0, on_exit=boom)
        w.run(ticks=1)  # must not propagate

    def test_rejects_non_callable_non_device(self):
        with self.assertRaises(ValueError):
            Watcher("not a callback", [])

    def test_accepts_a_device_like_object_for_the_cli(self):
        class FakeDev:
            def call(self, *a, **kw):   # never reached: nothing is applied
                raise AssertionError("appwatch must not talk to the device")

        w = Watcher(FakeDev(), {"rules": [], "restore_on_exit": True},
                    snapshot=lambda: [], foreground=lambda: None,
                    sleep=lambda _s: None)
        self.assertIsInstance(w.apply, DeviceApplier)
        self.assertIsNotNone(w.on_exit)
        w.on_exit()  # nothing applied yet, so restore is a no-op


class TestForegroundProbeParsing(unittest.TestCase):
    def test_first_json_object_from_eval_output(self):
        out = '(true, \'{"pid": 1234, "wm_class": "firefox"}\')\n'
        self.assertEqual(_first_json(out),
                         {"pid": 1234, "wm_class": "firefox"})

    def test_first_json_array_from_extension_output(self):
        out = ('(\'[{"wm_class": "Steam", "pid": 10, "focus": false}, '
               '{"wm_class": "cs2", "pid": 11, "focus": true}]\',)\n')
        windows = _first_json(out, prefer="array")
        self.assertEqual(len(windows), 2)
        self.assertTrue(windows[1]["focus"])

    def test_first_json_array_with_one_element(self):
        out = '(\'[{"wm_class": "cs2", "focus": true}]\',)\n'
        self.assertIsInstance(_first_json(out, prefer="array"), list)

    def test_first_json_on_garbage(self):
        self.assertIsNone(_first_json("(false, 'Eval disabled')"))

    def test_eval_disabled_reports_unavailable(self):
        p = ForegroundProbe(clock=Clock())
        p._gdbus = lambda *a: "(false, 'Eval is not allowed')\n"
        with self.assertRaises(_ProbeUnavailable):
            p._via_eval()

    def test_eval_resolves_wm_class_without_pid(self):
        p = ForegroundProbe(clock=Clock())
        p._gdbus = lambda *a: '(true, \'{"pid": 0, "wm_class": "Blender"}\')'
        self.assertEqual(p._via_eval(), Process.synthetic("blender"))

    def test_window_calls_picks_the_focused_window(self):
        p = ForegroundProbe(clock=Clock())
        p._gdbus = lambda *a: (
            '(\'[{"wm_class": "Steam", "pid": 0, "focus": false}, '
            '{"wm_class": "cs2", "pid": 0, "focus": true}]\',)')
        self.assertEqual(p._via_window_calls().comm, "cs2")

    def test_window_calls_with_nothing_focused(self):
        p = ForegroundProbe(clock=Clock())
        p._gdbus = lambda *a: '(\'[{"wm_class": "Steam", "focus": false}]\',)'
        self.assertIsNone(p._via_window_calls())

    def test_focused_window_extension_shape(self):
        p = ForegroundProbe(clock=Clock())
        p._gdbus = lambda *a: (
            '(\'{"title": "doc", "wm_class": "org.gnome.TextEditor"}\',)')
        self.assertEqual(p._via_focused_window().exe,
                         "org.gnome.texteditor")


class TestForegroundProbeFallback(unittest.TestCase):
    def test_all_unavailable_returns_none(self):
        clock = Clock()
        p = ForegroundProbe(clock=clock)
        tried: list[str] = []

        def dead(name):
            def fn():
                tried.append(name)
                raise _ProbeUnavailable(name)
            return fn
        p._strategies = [_Strategy("a", dead("a")), _Strategy("b", dead("b"))]
        self.assertIsNone(p.poll())
        self.assertFalse(p.available)
        self.assertEqual(tried, ["a", "b"])

    def test_failed_strategy_is_not_retried_immediately(self):
        clock = Clock()
        p = ForegroundProbe(retry=60.0, clock=clock)
        tried: list[str] = []

        def fn():
            tried.append("x")
            raise _ProbeUnavailable("nope")
        p._strategies = [_Strategy("x", fn)]
        p.poll()
        clock.advance(10.0)
        p.poll()
        self.assertEqual(len(tried), 1)   # still inside the backoff
        clock.advance(100.0)
        p.poll()
        self.assertEqual(len(tried), 2)

    def test_unexpected_exception_does_not_escape(self):
        p = ForegroundProbe(clock=Clock())

        def fn():
            raise KeyError("surprise from a shell extension")
        p._strategies = [_Strategy("x", fn)]
        self.assertIsNone(p.poll())

    def test_working_strategy_records_mechanism(self):
        p = ForegroundProbe(clock=Clock())
        p._strategies = [_Strategy("broken", lambda: (_ for _ in ()).throw(
                             _ProbeUnavailable("no"))),
                         _Strategy("good", lambda: proc("cs2"))]
        self.assertEqual(p.poll().comm, "cs2")
        self.assertEqual(p.mechanism, "good")

    def test_answered_but_nothing_focused(self):
        # An empty desktop is not a broken probe: no backoff, and the watcher
        # falls back to presence for this tick.
        p = ForegroundProbe(clock=Clock())
        p._strategies = [_Strategy("good", lambda: None)]
        self.assertIsNone(p.poll())
        self.assertEqual(p.mechanism, "good")
        self.assertEqual(p._strategies[0].next_try, 0.0)


class TestProcReaders(unittest.TestCase):
    """The /proc readers, against a temp tree rather than the real /proc."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.addCleanup(self.tmp.cleanup)

    def write_pid(self, pid: int, comm: str, argv: list[str]) -> None:
        d = os.path.join(self.root, str(pid))
        os.makedirs(d)
        with open(os.path.join(d, "comm"), "w") as fh:
            fh.write(comm + "\n")
        with open(os.path.join(d, "cmdline"), "wb") as fh:
            fh.write(b"".join(a.encode() + b"\0" for a in argv))

    def test_reads_comm_and_argv(self):
        self.write_pid(42, "CS2", ["/games/cs2/bin/cs2", "-novid"])
        p = read_process(42, self.root)
        self.assertEqual(p.comm, "cs2")          # lowercased
        self.assertEqual(p.exe, "cs2")           # from argv[0]: no exe symlink
        self.assertEqual(p.cmdline, "/games/cs2/bin/cs2 -novid")

    def test_missing_pid_is_none(self):
        self.assertIsNone(read_process(9999, self.root))

    def test_empty_cmdline_still_yields_comm(self):
        self.write_pid(7, "kworker/0:1", [])
        p = read_process(7, self.root)
        self.assertEqual((p.comm, p.exe, p.cmdline), ("kworker/0:1", "", ""))

    def test_snapshot_skips_non_pid_entries(self):
        self.write_pid(1, "systemd", ["/sbin/init"])
        self.write_pid(2, "blender", ["/usr/bin/blender"])
        os.makedirs(os.path.join(self.root, "sys"))
        with open(os.path.join(self.root, "uptime"), "w") as fh:
            fh.write("1 1\n")
        names = sorted(p.comm for p in snapshot_processes(self.root))
        self.assertEqual(names, ["blender", "systemd"])

    def test_snapshot_of_missing_root_is_empty(self):
        self.assertEqual(snapshot_processes(self.root + "/nope"), [])

    def test_snapshot_feeds_the_matcher(self):
        self.write_pid(11, "blender", ["/usr/bin/blender", "--factory"])
        rs = RuleSet(rules=[Rule(match=("blender",), profile=2)])
        a = rs.match(snapshot_processes(self.root))
        self.assertEqual(a.profile, 2)


if __name__ == "__main__":
    unittest.main()
