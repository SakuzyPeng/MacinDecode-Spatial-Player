"""Keep the conditions MacinRender's cross-platform bit equality rests on.

ADM Core gates its PCM as bit-identical on macOS arm64, Linux x64 and Windows
x64, and the player links that Rust code through Cargo. Cargo applies patches
only from the root workspace and unifies features across the whole graph, so
the player has to repeat Core's own `[patch.crates-io]`, and one unrelated
dependency could change what Core's code links. RustFFT compiles its SIMD
kernels, but Core's `mradm-scalar-default` keeps every default planner scalar;
only the realtime room quality asks for the native one (ADR 0015, 0020).
Check the graph the installers are built from, and that every pin names one
commit.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")
# Core's patched copies, by name: its rust/Cargo.toml `[patch.crates-io]`.
PATCHED = ("sofar", "rubato", "rustfft", "realfft", "ebur128", "rayon-core")
# Features each patched DSP crate must resolve to, exactly as Core builds it.
FEATURES = {"rustfft": "avx,mradm-scalar-default,neon,sse", "realfft": "", "rubato": ""}
CORE = "MacinRender-ADM-Core.git?rev="


def core_pin():
    text = (ROOT / "crates/macinrender/Cargo.toml").read_text(encoding="utf-8")
    return re.search(r'mradm-capi = \{ git = "[^"]+", rev = "([0-9a-f]{40})"', text).group(1)


def resolved(target):
    command = ["cargo", "tree", "--locked", "--offline", "--target", target, "-e", "normal",
               "--prefix", "none", "--format", "{p} [{f}]"]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if result.returncode:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            raise AssertionError(result.stderr)
        raise unittest.SkipTest("dependencies are not fetched: " + result.stderr.strip()[-200:])
    packages = {}
    for line in result.stdout.splitlines():
        match = re.match(r"(\S+) v\S+(?: \((\S+)\))?.*\[(.*)\]", line)
        if match:
            name, source, features = match.groups()
            packages.setdefault(name, set()).add((source or "", features))
    return packages


@unittest.skipUnless(shutil.which("cargo"), "Requires cargo")
class RenderDeterminismTests(unittest.TestCase):
    def test_renderer_dsp_keeps_core_scalar_and_portable_math(self):
        pin = core_pin()
        # Each CI job builds TARGET, so only that target's dependencies are
        # guaranteed to be cached. Local runs without TARGET check both graphs.
        targets = (os.environ["TARGET"],) if "TARGET" in os.environ else TARGETS
        for target in targets:
            self.assertIn(target, TARGETS, "Expected a packaged desktop target")
            packages = resolved(target)
            for name in PATCHED:
                with self.subTest(target=target, package=name):
                    self.assertIn(name, packages)
                    for source, features in packages[name]:
                        self.assertIn(CORE + pin, source, f"{name} must be Core's patched copy")
                        if name in FEATURES:
                            self.assertEqual(features, FEATURES[name], f"{name} features differ from Core's")
            with self.subTest(target=target, package="nalgebra"):
                self.assertTrue(all("libm-force" in features.split(",")
                                    for _, features in packages["nalgebra"]))
            for name in ("mradm-capi", "mradm-islands-sys"):
                with self.subTest(target=target, package=name):
                    self.assertIn(name, packages)
                    self.assertTrue(all(CORE + pin in source for source, _ in packages[name]))

    def test_the_workspace_repeats_every_patch_core_applies(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        patches = manifest["patch"]["crates-io"]
        self.assertEqual(sorted(patches), sorted(PATCHED))
        for name, patch in patches.items():
            with self.subTest(package=name):
                self.assertEqual(patch.get("rev"), core_pin(), f"{name} patch is not the linked Core")
        # Packaging's checkout of the same commit: a patch Core adds later
        # has to be added here too, or Core's code links the crates.io original.
        source = os.environ.get("MACINRENDER_SOURCE_DIR")
        if not source:
            self.skipTest("MACINRENDER_SOURCE_DIR is not prepared")
        core = tomllib.loads((Path(source) / "rust/Cargo.toml").read_text(encoding="utf-8"))
        self.assertEqual(sorted(core["patch"]["crates-io"]), sorted(PATCHED))

    def test_release_flags_do_not_target_the_build_machine(self):
        # target-cpu=native or +fma would let LLVM contract and vectorise by host.
        config = (ROOT / ".cargo/config.toml").read_text(encoding="utf-8")
        self.assertNotRegex(config, r"target-cpu|\+fma|\+avx")


if __name__ == "__main__":
    unittest.main()
