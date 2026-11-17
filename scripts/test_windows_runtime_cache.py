"""Check that signed-runtime keys track binary inputs, not unrelated edits."""

import contextlib
import io
import runpy
import tempfile
import unittest
from pathlib import Path


class RuntimeCacheKeyTest(unittest.TestCase):
    def test_key_tracks_pins_and_lock(self):
        scope = runpy.run_path(str(Path(__file__).with_name("build-windows-signed-runtime.py")))
        cache_key = scope["cache_key"]
        values = cache_key.__globals__

        def key():
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                cache_key()
            return output.getvalue().strip()

        original_source = values["SOURCE"]
        original_pinned = values["pinned"]
        original_lock = values["LOCK"]
        original_markitdown_lock = values["MARKITDOWN_LOCK"]
        try:
            baseline = key()
            values["SOURCE"] += "\n// unrelated change\n"
            self.assertEqual(key(), baseline)
            values["pinned"] = lambda name: original_pinned(name) + ("changed" if name == "PYTHON_STANDALONE_RELEASE" else "")
            self.assertNotEqual(key(), baseline)
            values["pinned"] = lambda name: original_pinned(name) + ("changed" if name == "MARKITDOWN_PINNED_VERSION" else "")
            self.assertNotEqual(key(), baseline)
            values["pinned"] = original_pinned
            with tempfile.TemporaryDirectory() as directory:
                lock = Path(directory) / "requirements.lock"
                lock.write_bytes(original_lock.read_bytes() + b"\n# changed\n")
                values["LOCK"] = lock
                self.assertNotEqual(key(), baseline)
                values["LOCK"] = original_lock
                markitdown_lock = Path(directory) / "markitdown.lock"
                markitdown_lock.write_bytes(original_markitdown_lock.read_bytes() + b"\n# changed\n")
                values["MARKITDOWN_LOCK"] = markitdown_lock
                self.assertNotEqual(key(), baseline)
        finally:
            values["SOURCE"] = original_source
            values["pinned"] = original_pinned
            values["LOCK"] = original_lock
            values["MARKITDOWN_LOCK"] = original_markitdown_lock

    def test_build_script_edits_rebuild_the_archive_but_keep_the_generation(self):
        scope = runpy.run_path(str(Path(__file__).with_name("build-windows-signed-runtime.py")))
        values = scope["cache_key"].__globals__

        def printed(command):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                scope[command]()
            return output.getvalue().strip()

        original_scripts = values["BUILD_SCRIPTS"]
        original_launcher = values["LAUNCHER"]
        original_layout = values["LAYOUT"]
        try:
            cache, generation = printed("cache_key"), printed("generation")
            self.assertNotEqual(cache, generation)
            with tempfile.TemporaryDirectory() as directory:
                script = Path(directory) / "build.ps1"
                script.write_bytes(original_scripts[0].read_bytes() + b"\n# faster signing\n")
                values["BUILD_SCRIPTS"] = (script, original_scripts[1])
                self.assertNotEqual(printed("cache_key"), cache)
                self.assertEqual(printed("generation"), generation)
                values["BUILD_SCRIPTS"] = original_scripts
                launcher = Path(directory) / "launcher.rs"
                launcher.write_bytes(original_launcher.read_bytes() + b"\n// changed\n")
                values["LAUNCHER"] = launcher
                self.assertNotEqual(printed("generation"), generation)
                self.assertNotEqual(printed("cache_key"), cache)
                values["LAUNCHER"] = original_launcher
            values["LAYOUT"] = original_layout + "1"
            self.assertNotEqual(printed("generation"), generation)
        finally:
            values["BUILD_SCRIPTS"] = original_scripts
            values["LAUNCHER"] = original_launcher
            values["LAYOUT"] = original_layout


if __name__ == "__main__":
    unittest.main()
