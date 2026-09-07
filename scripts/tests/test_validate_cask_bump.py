import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "validate-cask-bump.py"
SPEC = importlib.util.spec_from_file_location("validate_cask_bump", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VALIDATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATOR)

OLD_HASHES = ["1" * 64, "2" * 64, "3" * 64]
NEW_HASHES = ["a" * 64, "b" * 64, "c" * 64]
BASE = f'''cask "midden" do
  version "1.0.0"
  sha256 "{OLD_HASHES[0]}"
  sha256 arm64_linux: "{OLD_HASHES[1]}",
         x86_64_linux: "{OLD_HASHES[2]}"
  binary "midden"
end
'''
CANDIDATE = (
    BASE.replace('version "1.0.0"', 'version "1.1.0"')
    .replace(OLD_HASHES[0], NEW_HASHES[0])
    .replace(OLD_HASHES[1], NEW_HASHES[1])
    .replace(OLD_HASHES[2], NEW_HASHES[2])
)


class ValidateCaskBumpTests(unittest.TestCase):
    def test_renders_an_exact_version_and_checksum_bump(self) -> None:
        self.assertEqual(VALIDATOR.render(BASE, "1.1.0", NEW_HASHES), CANDIDATE)

    def test_same_version_still_reconciles_wrong_checksums(self) -> None:
        candidate = VALIDATOR.render(BASE, "1.0.0", NEW_HASHES)
        self.assertNotEqual(candidate, BASE)
        VALIDATOR.validate(BASE, candidate, "1.0.0", NEW_HASHES)

    def test_accepts_exact_version_and_checksum_bump(self) -> None:
        VALIDATOR.validate(BASE, CANDIDATE, "1.1.0", NEW_HASHES)

    def test_rejects_unrelated_candidate_code(self) -> None:
        malicious = CANDIDATE.replace(
            '  binary "midden"', '  preflight { system "curl", "example.test" }\n  binary "midden"'
        )
        with self.assertRaisesRegex(ValueError, "content other than"):
            VALIDATOR.validate(BASE, malicious, "1.1.0", NEW_HASHES)

    def test_rejects_unverified_checksum(self) -> None:
        with self.assertRaisesRegex(ValueError, "do not match"):
            VALIDATOR.validate(BASE, CANDIDATE, "1.1.0", ["d" * 64, *NEW_HASHES[1:]])

    def test_rejects_nonliteral_version_logic(self) -> None:
        candidate = CANDIDATE.replace('version "1.1.0"', 'version ENV.fetch("VERSION")')
        with self.assertRaisesRegex(ValueError, "candidate version"):
            VALIDATOR.validate(BASE, candidate, "1.1.0", NEW_HASHES)


if __name__ == "__main__":
    unittest.main()
