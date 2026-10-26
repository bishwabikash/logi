"""Offline tests for g502.config. No device, no real ~/.config."""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from collections.abc import Mapping
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from g502 import config as cfgmod
from g502.config import (Config, ConfigError, LedPref, ProfilePrefs,
                         SCHEMA_VERSION)


class TempConfig(unittest.TestCase):
    """Base: every test gets its own directory, never the user's config."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="g502-test-")
        self.dir = Path(self._tmp.name)
        self.path = self.dir / "config.json"
        self.addCleanup(self._tmp.cleanup)

    def write(self, text: str) -> Path:
        self.path.write_text(text, encoding="utf-8")
        return self.path

    def raw(self) -> dict:
        return json.loads(self.path.read_text(encoding="utf-8"))

    def stray(self) -> list[str]:
        return sorted(p.name for p in self.dir.iterdir()
                      if p.name != "config.json")


class TestPath(TempConfig):
    def test_xdg_config_home_honoured(self) -> None:
        with mock.patch.dict(os.environ,
                             {"XDG_CONFIG_HOME": "/xdg/base"}, clear=False):
            self.assertEqual(cfgmod.config_path(),
                             Path("/xdg/base/g502/config.json"))

    def test_falls_back_to_dot_config(self) -> None:
        env = {k: v for k, v in os.environ.items() if k != "XDG_CONFIG_HOME"}
        with mock.patch.dict(os.environ, env, clear=True):
            self.assertEqual(cfgmod.config_path(),
                             Path.home() / ".config/g502/config.json")

    def test_empty_and_relative_xdg_ignored(self) -> None:
        fallback = Path.home() / ".config/g502/config.json"
        for value in ("", "relative/dir"):
            with mock.patch.dict(os.environ,
                                 {"XDG_CONFIG_HOME": value}, clear=False):
                self.assertEqual(cfgmod.config_path(), fallback)


class TestLoadDegradation(TempConfig):
    def test_missing_file_gives_clean_defaults(self) -> None:
        cfg = cfgmod.load(self.dir / "nope.json")
        self.assertEqual(cfg, Config())
        self.assertEqual(cfg.warnings, [])          # absence is not a problem
        self.assertEqual(cfg.path, self.dir / "nope.json")
        self.assertFalse(self.path.exists())        # load must not create it

    def test_corrupt_json_does_not_raise_or_clobber(self) -> None:
        text = '{"default_profile": 3, "rules": [{"match": ["x"]'
        self.write(text)
        cfg = cfgmod.load(self.path)
        self.assertEqual(cfg, Config())
        self.assertTrue(cfg.warnings)
        self.assertIn("invalid JSON", cfg.warnings[0])
        # A broken file is reported, not repaired behind the user's back.
        self.assertEqual(self.path.read_text(encoding="utf-8"), text)

    def test_empty_file(self) -> None:
        self.write("")
        cfg = cfgmod.load(self.path)
        self.assertEqual(cfg, Config())
        self.assertIn("empty", cfg.warnings[0])

    def test_whitespace_only_file(self) -> None:
        self.write("\n  \n")
        self.assertEqual(cfgmod.load(self.path), Config())

    def test_non_object_top_level(self) -> None:
        self.write("[1, 2, 3]")
        cfg = cfgmod.load(self.path)
        self.assertEqual(cfg, Config())
        self.assertIn("expected an object", cfg.warnings[0])

    def test_unreadable_path(self) -> None:
        cfg = cfgmod.load(self.dir)  # a directory, not a file
        self.assertEqual(cfg, Config())
        self.assertIn("unreadable", cfg.warnings[0])

    def test_partially_invalid_keeps_the_valid_parts(self) -> None:
        self.write(json.dumps({
            "default_profile": "two",       # wrong type
            "restore_on_exit": "yes",       # wrong type
            "effect_fps": 9000,             # out of range
            "effect": 42,                   # wrong type
            "rules": {"not": "a list"},     # wrong container
            "profiles": {
                "0": {"dpi": 1600, "rate": "fast",
                      "leds": [{"mode": "ripple", "color": "#112233"},
                               "junk"]},
                "bogus": {"dpi": 400},      # unusable key
                "2": ["not", "an", "object"],
            },
        }))
        cfg = cfgmod.load(self.path)
        d = Config()
        self.assertEqual(cfg.default_profile, d.default_profile)
        self.assertEqual(cfg.restore_on_exit, d.restore_on_exit)
        self.assertEqual(cfg.effect_fps, d.effect_fps)
        self.assertIsNone(cfg.effect)
        self.assertEqual(cfg.rules, [])
        self.assertEqual(list(cfg.profiles), [0])
        self.assertEqual(cfg.profiles[0].dpi, 1600)      # the good value stays
        self.assertIsNone(cfg.profiles[0].rate)
        self.assertEqual(len(cfg.profiles[0].leds), 1)
        self.assertEqual(cfg.profiles[0].leds[0].mode, "fixed")  # not a mode
        self.assertEqual(cfg.profiles[0].leds[0].color, "#112233")
        self.assertGreaterEqual(len(cfg.warnings), 9)

    def test_load_survives_a_hostile_shape(self) -> None:
        # Deeply pathological but valid JSON must still not raise.
        self.write(json.dumps({"profiles": {"0": {"leds": [[[{}]]]}},
                               "effect_params": [1, 2],
                               "version": True}))
        self.assertIsInstance(cfgmod.load(self.path), Config)


class TestRoundTrip(TempConfig):
    def full_config(self) -> Config:
        return Config(
            default_profile=2,
            restore_on_exit=False,
            effect="rainbow",
            effect_params={"period": 4.5, "zone_offset": 0.3},
            effect_fps=90,
            profiles={
                0: ProfilePrefs(dpi=1600, rate=1000,
                                leds=[LedPref(color="#00aaff"),
                                      LedPref(mode="breathing",
                                              color="#ff00ff",
                                              period_ms=3000,
                                              waveform="sine",
                                              intensity=70)]),
                4: ProfilePrefs(dpi=400),
            },
            rules=[{"match": ["cs2"], "profile": 1},
                   {"match": ["blender"], "profile": 2, "dpi": 1200}],
        )

    def test_save_load_is_identity(self) -> None:
        original = self.full_config()
        cfgmod.save(original, self.path)
        reloaded = cfgmod.load(self.path)
        self.assertEqual(reloaded.warnings, [])
        self.assertEqual(reloaded, original)

    def test_second_round_trip_is_byte_identical(self) -> None:
        cfgmod.save(self.full_config(), self.path)
        first = self.path.read_bytes()
        cfgmod.save(cfgmod.load(self.path), self.path)
        self.assertEqual(self.path.read_bytes(), first)

    def test_defaults_round_trip(self) -> None:
        cfgmod.save(Config(), self.path)
        self.assertEqual(cfgmod.load(self.path), Config())

    def test_shipped_default_and_example_dicts_load(self) -> None:
        for sample in (cfgmod.DEFAULT_CONFIG, cfgmod.EXAMPLE_CONFIG):
            cfgmod.save(sample, self.path)
            cfg = cfgmod.load(self.path)
            self.assertEqual(cfg.warnings, [])
            self.assertEqual(cfg.to_dict(), sample)

    def test_mapping_access_for_appwatch(self) -> None:
        cfg = self.full_config()
        self.assertEqual(cfg.get("rules"), cfg.rules)
        self.assertEqual(cfg.get("default_profile", 0), 2)
        self.assertIs(cfg.get("missing", True), True)
        self.assertEqual(cfg["restore_on_exit"], False)

    def test_prefs_for_creates_on_demand(self) -> None:
        cfg = Config()
        cfg.prefs_for(3).dpi = 800
        self.assertEqual(cfg.profiles[3].dpi, 800)
        cfgmod.save(cfg, self.path)
        self.assertEqual(cfgmod.load(self.path).profiles[3].dpi, 800)


class TestUnknownKeys(TempConfig):
    def test_unknown_keys_survive_a_round_trip(self) -> None:
        self.write(json.dumps({
            "version": SCHEMA_VERSION,
            "default_profile": 1,
            "future_toggle": {"nested": [1, 2, 3]},
            "profiles": {"0": {"dpi": 800, "future_pref": "keep me",
                               "leds": [{"mode": "fixed", "color": "#010203",
                                         "future_led": 7}]}},
        }))
        cfg = cfgmod.load(self.path)
        cfgmod.save(cfg, self.path)
        out = self.raw()
        self.assertEqual(out["future_toggle"], {"nested": [1, 2, 3]})
        self.assertEqual(out["profiles"]["0"]["future_pref"], "keep me")
        self.assertEqual(out["profiles"]["0"]["leds"][0]["future_led"], 7)
        self.assertEqual(out["default_profile"], 1)   # known keys still right
        self.assertEqual(out["profiles"]["0"]["dpi"], 800)

    def test_known_keys_win_over_stale_extras(self) -> None:
        cfg = Config(default_profile=3)
        cfg.extra["default_profile"] = 99  # hand-stuffed; must not shadow
        self.assertEqual(cfg.to_dict()["default_profile"], 3)


class TestFutureVersion(TempConfig):
    def test_future_version_is_reported_not_mangled(self) -> None:
        payload = {"version": SCHEMA_VERSION + 5, "default_profile": 1,
                   "tomorrows_key": "value"}
        self.write(json.dumps(payload))
        cfg = cfgmod.load(self.path)
        self.assertTrue(cfg.is_future)
        self.assertEqual(cfg.version, SCHEMA_VERSION + 5)
        self.assertTrue(any("newer than" in w for w in cfg.warnings))
        self.assertEqual(cfg.extra["tomorrows_key"], "value")

        with self.assertRaises(ConfigError):
            cfgmod.save(cfg, self.path)
        self.assertEqual(self.raw(), payload)  # untouched by the refusal

        cfgmod.save(cfg, self.path, force=True)
        self.assertEqual(self.raw()["tomorrows_key"], "value")


class TestMigrate(unittest.TestCase):
    def test_version_1_is_a_no_op(self) -> None:
        data, notes = cfgmod.migrate({"version": 1, "default_profile": 2})
        self.assertEqual(data, {"version": 1, "default_profile": 2})
        self.assertEqual(notes, [])

    def test_missing_version_assumed_current(self) -> None:
        data, notes = cfgmod.migrate({"default_profile": 2})
        self.assertEqual(data["version"], SCHEMA_VERSION)
        self.assertEqual(notes, [])

    def test_older_version_without_a_step_is_reported(self) -> None:
        data, notes = cfgmod.migrate({"version": 1}, from_version=0)
        self.assertEqual(data["version"], 0)  # no false claim of being current
        self.assertTrue(any("no migration" in n for n in notes))

    def test_registered_step_runs(self) -> None:
        def bump(d: dict) -> tuple[dict, list[str]]:
            d["renamed"] = d.pop("old_name")
            return d, ["renamed old_name -> renamed"]

        with mock.patch.dict(cfgmod.MIGRATIONS, {0: bump}, clear=False):
            data, notes = cfgmod.migrate({"version": 0, "old_name": 5})
        self.assertEqual(data, {"version": SCHEMA_VERSION, "renamed": 5})
        self.assertEqual(notes, ["renamed old_name -> renamed"])

    def test_future_version_passes_through_untouched(self) -> None:
        src = {"version": SCHEMA_VERSION + 1, "unknown": [1]}
        data, notes = cfgmod.migrate(src)
        self.assertEqual(data, src)
        self.assertTrue(notes)


class TestAtomicSave(TempConfig):
    def test_leaves_no_temp_files(self) -> None:
        cfgmod.save(Config(default_profile=1), self.path)
        self.assertEqual(self.stray(), [])

    def test_creates_parent_directories(self) -> None:
        nested = self.dir / "a" / "b" / "config.json"
        self.assertEqual(cfgmod.save(Config(), nested), nested)
        self.assertTrue(nested.is_file())

    def test_failed_write_keeps_the_previous_file(self) -> None:
        cfgmod.save(Config(default_profile=1), self.path)
        before = self.path.read_bytes()
        with self.assertRaises(TypeError):
            cfgmod.save({"version": SCHEMA_VERSION, "bad": object()},
                        self.path)
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(self.stray(), [])  # partial file cleaned up

    def test_interrupted_write_keeps_the_previous_file(self) -> None:
        cfgmod.save(Config(default_profile=1), self.path)
        before = self.path.read_bytes()
        with mock.patch("g502.config.os.replace",
                        side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                cfgmod.save(Config(default_profile=4), self.path)
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(self.stray(), [])

    def test_replace_is_the_only_publishing_step(self) -> None:
        # Nothing may be written through the real path: readers of a config
        # being rewritten must never see a truncated file.
        calls: list[tuple[str, str]] = []
        real_replace = cfgmod.os.replace

        def spy(src, dst):
            calls.append((str(src), str(dst)))
            return real_replace(src, dst)

        with mock.patch("g502.config.os.replace", side_effect=spy):
            cfgmod.save(Config(), self.path)
        self.assertEqual(len(calls), 1)
        self.assertNotEqual(calls[0][0], str(self.path))
        self.assertEqual(calls[0][1], str(self.path))

    def test_overwrites_a_longer_file_completely(self) -> None:
        self.write(json.dumps({"version": 1, "junk": "x" * 4096}))
        cfgmod.save(Config(), self.path)
        self.assertEqual(self.raw(), Config().to_dict())

    def test_path_defaults_to_the_one_loaded_from(self) -> None:
        cfgmod.save(Config(default_profile=2), self.path)
        cfg = cfgmod.load(self.path)
        cfg.default_profile = 3
        self.assertEqual(cfgmod.save(cfg), self.path)  # no path argument
        self.assertEqual(self.raw()["default_profile"], 3)

    def test_mode_is_readable(self) -> None:
        # mkstemp is 0600; a config written under sudo must stay user-readable.
        cfgmod.save(Config(), self.path)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o644)

    def test_trailing_newline(self) -> None:
        cfgmod.save(Config(), self.path)
        self.assertTrue(self.path.read_text(encoding="utf-8").endswith("\n"))


class TestAppwatchInterop(TempConfig):
    """The rule schema lives in appwatch; config only has to carry it."""

    def test_config_is_a_mapping(self) -> None:
        cfg = Config(default_profile=1)
        self.assertIsInstance(cfg, Mapping)       # appwatch isinstance-checks
        self.assertEqual(dict(cfg), cfg.to_dict())
        self.assertIn("rules", cfg)

    def test_ruleset_survives_a_round_trip(self) -> None:
        appwatch = _import_appwatch()
        if appwatch is None or not hasattr(appwatch, "RuleSet"):
            self.skipTest("appwatch.RuleSet not available")
        rs = appwatch.RuleSet(
            rules=[appwatch.Rule.from_dict({"match": ["cs2"], "profile": 1}),
                   appwatch.Rule.from_dict({"cmdline": ["blender"],
                                            "profile": 2, "dpi": 1200,
                                            "name": "3d"})],
            default_profile=3)
        cfg = Config.from_dict(rs.to_config())
        self.assertEqual(cfg.warnings, [])
        cfgmod.save(cfg, self.path)
        reloaded = cfgmod.load(self.path)
        self.assertEqual(appwatch.RuleSet.from_config(reloaded), rs)


def _import_appwatch():
    """Lazy and optional: appwatch is a sibling module under concurrent work."""
    try:
        from g502 import appwatch
    except ImportError:
        return None
    return appwatch


if __name__ == "__main__":
    unittest.main()
