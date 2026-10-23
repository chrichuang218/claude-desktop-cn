"""Exercise the Mac adapter without invoking an installed Claude application."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "src-tauri/src/macos/engine_adapter.py"
UPSTREAM = Path(os.environ.get(
    "CLAUDE_UPSTREAM_SCRIPT", ROOT / ".work/macos-upstream/patch_claude_zh_cn.py"
))
FAKE_UPSTREAM = '''
import argparse
import json
from pathlib import Path
import tempfile

Path(__file__).with_suffix(".imported").write_text("imported")

def quit_claude():
    raise AssertionError("must not close real Claude from the engine")

def set_user_locale(*args, **kwargs):
    raise AssertionError("must not change the real user config from the engine")

def verify(*args, **kwargs):
    pass

def backup_and_replace(*args, **kwargs):
    pass

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", required=True)
    parser.add_argument("--user-home", required=True)
    parser.add_argument("--lang", required=True)
    parser.add_argument("--skip-asar-patch", action="store_true")
    args = parser.parse_args()
    quit_claude()
    set_user_locale(Path(args.user_home), args.lang)
    print(json.dumps(dict(vars(args), temporary=tempfile.gettempdir())))
    return 0
'''


class MacEngineAdapterTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="claude-adapter-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.work = self.root / "isolated app copy"
        self.work.mkdir()
        self.app = self.work / "Claude.app"
        self.app.mkdir()
        self.script = self.root / "upstream.py"
        self.script.write_text(FAKE_UPSTREAM, encoding="utf-8")

    def invoke(self, mode, app=None):
        return subprocess.run(
            [sys.executable, "-X", "utf8", str(ADAPTER), str(self.script),
             str(app or self.app), str(self.work), mode],
            capture_output=True, text=True, encoding="utf-8", timeout=15,
        )

    def check_mode(self, mode, skip_asar):
        result = self.invoke(mode)
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads(result.stdout)
        self.assertEqual(args["app"], str(self.app.resolve()))
        self.assertEqual(args["user_home"], str(self.work.resolve()))
        self.assertEqual(args["lang"], "zh-CN")
        self.assertEqual(args["skip_asar_patch"], skip_asar)
        self.assertEqual(Path(args["temporary"]), self.work.resolve() / "temporary")

    def test_safe_mode_restricts_target_and_disables_upstream_side_effects(self):
        self.check_mode("safe", True)

    def test_invalid_targets_and_mode_fail_before_importing_upstream(self):
        outside = self.root / "Claude.app"
        wrong_name = self.work / "Other.app"
        nested = self.app / "Claude.app"
        for app in (outside, wrong_name, nested):
            app.mkdir()
        cases = [(outside, "safe"), (wrong_name, "safe"), (nested, "safe"),
                 (self.app, "unknown"), (self.app, "full"), (self.work / "missing.app", "safe")]
        for app, mode in cases:
            with self.subTest(app=app, mode=mode):
                result = self.invoke(mode, app)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.script.with_suffix(".imported").exists())

    def test_symlink_cannot_escape_the_isolated_directory(self):
        outside = self.root / "outside" / "Claude.app"
        outside.mkdir(parents=True)
        self.app.rmdir()
        try:
            self.app.symlink_to(outside, target_is_directory=True)
        except OSError as error:
            if os.name != "nt":
                raise
            self.skipTest(f"Windows cannot create directory symlinks (error {error.winerror})")
        result = self.invoke("safe")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.script.with_suffix(".imported").exists())

    def test_returned_nonzero_exit_code_is_preserved(self):
        self.script.write_text(FAKE_UPSTREAM.replace("return 0", "return 17"), encoding="utf-8")
        self.assertEqual(self.invoke("safe").returncode, 17)

    def test_raised_nonzero_exit_code_is_preserved(self):
        self.script.write_text(FAKE_UPSTREAM.replace("return 0", "raise SystemExit(23)"), encoding="utf-8")
        self.assertEqual(self.invoke("safe").returncode, 23)

    def test_missing_upstream_interface_is_reported(self):
        self.script.write_text(FAKE_UPSTREAM.replace("def verify(", "def removed_verify("), encoding="utf-8")
        result = self.invoke("safe")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Upstream interface changed: verify", result.stderr)

    def test_archive_extraction_rejects_unsafe_entries_even_with_python_optimization(self):
        for name in ("../escape", "/absolute", "root/../../escape", "root\\escape", "root/link"):
            with self.subTest(name=name):
                archive = self.root / "unsafe.zip"
                with zipfile.ZipFile(archive, "w") as source:
                    entry = zipfile.ZipInfo(name)
                    # ZipInfo normalizes the host separator on Windows; retain
                    # the actual unsafe filename in the archive fixture.
                    entry.filename = name
                    if name.endswith("link"):
                        entry.create_system = 3
                        entry.external_attr = 0o120777 << 16
                    source.writestr(entry, "outside")
                destination = self.root / "extracted"
                result = subprocess.run(
                    [sys.executable, "-O", str(ADAPTER), "--extract", str(archive), str(destination)],
                    capture_output=True, timeout=15,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(destination.exists())
        archive = self.root / "valid.zip"
        with zipfile.ZipFile(archive, "w") as source:
            source.writestr("upstream/resources/example.json", '{"valid":true}')
        destination = self.root / "extracted"
        result = subprocess.run(
            [sys.executable, "-O", str(ADAPTER), "--extract", str(archive), str(destination)],
            capture_output=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((destination / "upstream/resources/example.json").read_text(), '{"valid":true}')
        again = subprocess.run(
            [sys.executable, str(ADAPTER), "--extract", str(archive), str(destination)],
            capture_output=True, timeout=15,
        )
        self.assertNotEqual(again.returncode, 0)

    @unittest.skipUnless(UPSTREAM.is_file(), "Set CLAUDE_UPSTREAM_SCRIPT to check a downloaded upstream script")
    def test_real_upstream_import_and_argument_contract_without_app_operations(self):
        class Parsed(Exception):
            pass

        parse_args = argparse.ArgumentParser.parse_args
        parsed = []

        def capture_args(parser, *args, **kwargs):
            parsed.append(parse_args(parser, *args, **kwargs))
            # Stop at the parser: no upstream app/config/permission code runs.
            raise Parsed()

        forbidden = AssertionError("Real upstream must not start a process in this check")
        with patch("subprocess.run", side_effect=forbidden), patch("subprocess.Popen", side_effect=forbidden):
            spec = importlib.util.spec_from_file_location("upstream_contract", UPSTREAM)
            upstream = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(upstream)
            for name in ("main", "quit_claude", "set_user_locale", "verify", "backup_and_replace"):
                self.assertTrue(callable(getattr(upstream, name, None)), name)
            argv = [str(UPSTREAM), "--app", str(self.app), "--user-home", str(self.work), "--lang", "zh-CN", "--skip-asar-patch"]
            with patch.object(sys, "argv", argv), patch.object(argparse.ArgumentParser, "parse_args", capture_args):
                with self.assertRaises(Parsed):
                    upstream.main()
            self.assertEqual(parsed[-1].app, self.app)
            self.assertEqual(parsed[-1].user_home, self.work)
            self.assertEqual(parsed[-1].lang, "zh-CN")
            self.assertTrue(parsed[-1].skip_asar_patch)


if __name__ == "__main__":
    unittest.main()
