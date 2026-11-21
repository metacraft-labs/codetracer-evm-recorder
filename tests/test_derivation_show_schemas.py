"""Parse the real `nix derivation show` output of every Nix that enters this shell.

The hook factory reads the derivation that built the declared Repro package
(tools/repro_hook_factory.py, `selected_derivation_body`). Its JSON shape has
changed four times across the Nix releases the dev shell is entered with, and
the shell hook takes `nix` from PATH, so which shape arrives depends on who
enters the shell: Repro 0.2.17 drives Nix 2.28.5, a NixOS host may carry 2.34.

No mock or synthetic Nix is used. Each file in tests/fixtures/nix-derivation-show/
is the unedited stdout of `nix derivation show` run by the named Nix release
against the same derivation, the deriver of the reprobuild 0.2.5 package this
repository's flake.lock selects. They were captured on 2026-10-31 on
x86_64-linux with:

    /nix/store/<hash>-nix-<version>/bin/nix --extra-experimental-features nix-command \\
        derivation show /nix/store/65vywkryknxdjvp3xipy47ykz303y5my-reprobuild-0.2.5.drv

The refusal cases below are those captures with one property moved from one
schema to another, so each checks that a shape mixing two schemas is refused.
"""
from __future__ import annotations
import copy
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("repro_hook_factory", ROOT / "tools/repro_hook_factory.py")
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("missing actual hook factory module")
FACTORY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FACTORY)

FIXTURES = ROOT / "tests/fixtures/nix-derivation-show"
DRV = Path("/nix/store/65vywkryknxdjvp3xipy47ykz303y5my-reprobuild-0.2.5.drv")
OUT = Path("/nix/store/kx1sdp8m39k6zwppr6rl51m1zz54a3cv-reprobuild-0.2.5")
SRC = "/nix/store/y5dpa167wb3ks3s08bfkfjjqgghfwq0h-0khgvbgqg810jb0s32vqxdxvlq81bs2j-source"

# Nix release -> schema its `nix derivation show` printed for DRV.
CAPTURED = {
    "2.28.5": "legacy",
    "2.31.5": "legacy",
    "2.32.6": "v3",
    "2.33pre20251107_479b6b73": "v4",
    "2.34.7": "v4",
    "2.35pre20260504_53ab7375": "v4",
}


def capture(version: str):
    return json.loads((FIXTURES / f"nix-{version}.json").read_text())


def only_body(decoded):
    entries = decoded["derivations"] if "derivations" in decoded else decoded
    return next(iter(entries.values()))


class DerivationShowSchemas(unittest.TestCase):
    def test_every_capture_is_present(self) -> None:
        self.assertEqual({p.name for p in FIXTURES.iterdir()}, {f"nix-{v}.json" for v in CAPTURED})

    def test_every_captured_nix_selects_the_same_constructor(self) -> None:
        for version, schema in CAPTURED.items():
            with self.subTest(nix=version):
                decoded = capture(version)
                body = FACTORY.selected_derivation_body(decoded, DRV)
                self.assertEqual(FACTORY.derivation_schema(body, "derivations" in decoded), schema)
                # What the factory goes on to read from the body.
                self.assertEqual(body["env"]["src"], SRC)
                self.assertIn(body["outputs"]["out"]["path"], (OUT.name, str(OUT)))

    def test_envelope_is_reported_only_by_v4(self) -> None:
        self.assertIn("derivations", capture("2.34.7"))
        self.assertNotIn("derivations", capture("2.33pre20251107_479b6b73"))

    def assert_refused(self, decoded, message: str) -> None:
        with self.assertRaises(RuntimeError) as raised:
            FACTORY.selected_derivation_body(decoded, DRV)
        self.assertIn(message, str(raised.exception))

    def test_legacy_body_under_a_basename_is_refused(self) -> None:
        legacy = capture("2.28.5")
        self.assert_refused({DRV.name: only_body(legacy)}, "does not name the exact selected derivation")

    def test_versioned_body_under_an_absolute_name_is_refused(self) -> None:
        for version in ("2.32.6", "2.34.7"):
            with self.subTest(nix=version):
                self.assert_refused({str(DRV): only_body(capture(version))}, "does not name the exact selected derivation")

    def test_legacy_and_v3_bodies_inside_the_envelope_are_refused(self) -> None:
        for version in ("2.28.5", "2.32.6"):
            with self.subTest(nix=version):
                body = only_body(capture(version))
                name = DRV.name if version == "2.32.6" else str(DRV)
                self.assert_refused({"version": 4, "derivations": {name: body}}, "inside a version envelope")

    def test_inputs_of_another_schema_are_refused(self) -> None:
        v4 = only_body(capture("2.34.7"))
        legacy = only_body(capture("2.28.5"))
        mixed = copy.deepcopy(v4)
        mixed["inputDrvs"] = legacy["inputDrvs"]
        self.assert_refused({DRV.name: mixed}, "unsupported package constructor derivation JSON")
        unversioned_v4 = copy.deepcopy(v4)
        del unversioned_v4["version"]
        self.assert_refused({str(DRV): unversioned_v4}, "unsupported package constructor derivation JSON")
        v3_with_inputs = copy.deepcopy(only_body(capture("2.32.6")))
        v3_with_inputs["inputs"] = v4["inputs"]
        self.assert_refused({DRV.name: v3_with_inputs}, "unsupported package constructor derivation JSON")

    def test_unknown_body_version_is_refused(self) -> None:
        body = copy.deepcopy(only_body(capture("2.34.7")))
        body["version"] = 5
        self.assert_refused({"version": 4, "derivations": {DRV.name: body}}, "body version 5")
        body["version"] = "4"
        self.assert_refused({DRV.name: body}, "body version is not an integer")

    def test_output_spelling_of_another_schema_is_refused(self) -> None:
        legacy = copy.deepcopy(capture("2.28.5"))
        only_body(legacy)["outputs"]["out"]["path"] = OUT.name
        self.assert_refused(legacy, "output path spelling disagrees with its legacy schema")
        v4 = copy.deepcopy(capture("2.34.7"))
        only_body(v4)["outputs"]["out"]["path"] = str(OUT)
        self.assert_refused(v4, "output path spelling disagrees with its v4 schema")

    def test_unknown_envelope_and_ambiguity_are_refused(self) -> None:
        v4 = capture("2.34.7")
        self.assert_refused({"version": 5, "derivations": v4["derivations"]}, "unsupported package constructor JSON envelope")
        self.assert_refused({"version": 4}, "unsupported package constructor JSON envelope")
        body = only_body(v4)
        self.assert_refused({"version": 4, "derivations": {DRV.name: body, "x-other.drv": body}}, "ambiguous package constructor")
        self.assert_refused({}, "ambiguous package constructor")
        self.assert_refused([], "is not an object")


if __name__ == "__main__":
    unittest.main()
