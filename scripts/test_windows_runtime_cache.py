"""Check that signed-runtime cache keys track binary inputs, not unrelated Rust edits."""

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
        try:
            baseline = key()
            values["SOURCE"] += "\n// unrelated change\n"
            self.assertEqual(key(), baseline)
            values["pinned"] = lambda name: original_pinned(name) + ("changed" if name == "PYTHON_STANDALONE_RELEASE" else "")
            self.assertNotEqual(key(), baseline)
            values["pinned"] = original_pinned
            with tempfile.TemporaryDirectory() as directory:
                lock = Path(directory) / "requirements.lock"
                lock.write_bytes(original_lock.read_bytes() + b"\n# changed\n")
                values["LOCK"] = lock
                self.assertNotEqual(key(), baseline)
        finally:
            values["SOURCE"] = original_source
            values["pinned"] = original_pinned
            values["LOCK"] = original_lock


if __name__ == "__main__":
    unittest.main()
