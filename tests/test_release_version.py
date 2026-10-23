"""Compile the real version helper with each build-time input, without GTK."""

import os
from pathlib import Path, PureWindowsPath
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def probe_source(module_path):
    return (
        f'#[path = "{module_path.as_posix()}"] mod release_version;\n'
        'fn main() { println!("{:?}", release_version::bundle_version()); }\n'
    )


class ReleaseVersionTests(unittest.TestCase):
    def test_windows_module_path(self):
        source = probe_source(
            PureWindowsPath(r"C:\Users\alice\aw-tauri\src-tauri\src\release_version.rs")
        )
        self.assertIn(
            '#[path = "C:/Users/alice/aw-tauri/src-tauri/src/release_version.rs"]',
            source,
        )

    def test_compiled_release_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            harness = Path(tmp) / "probe.rs"
            harness.write_text(probe_source(ROOT / "src-tauri/src/release_version.rs"))
            for value in (None, "", "v0.14.2", "v0.14.0b5", "v0.14.0b5-research"):
                with self.subTest(value=value):
                    env = os.environ.copy()
                    env.pop("AW_VERSION", None)
                    if value is not None:
                        env["AW_VERSION"] = value
                    binary = Path(tmp) / "probe"
                    subprocess.run(
                        ["rustc", str(harness), "-o", str(binary)],
                        env=env,
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    result = subprocess.check_output([str(binary)], text=True).strip()
                    expected = f'Some("{value}")' if value else "None"
                    self.assertEqual(result, expected)

    def test_version_is_set_before_mode_dispatch(self):
        source = (ROOT / "src-tauri/src/lib.rs").read_text()
        startup = source[source.index("pub fn run() {") :]
        self.assertLess(
            startup.index("aw_server::version::set_version_override(version)"),
            startup.index("if cli_args.daemon"),
        )
        self.assertLess(
            startup.index("release_version::bundle_version()"),
            startup.index("if cli_args.mini"),
        )


if __name__ == "__main__":
    unittest.main()
