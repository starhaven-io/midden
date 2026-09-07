import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "format-release-notes.py"
SPEC = importlib.util.spec_from_file_location("format_release_notes", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
FORMATTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FORMATTER)


class ReleaseNotesTests(unittest.TestCase):
    def test_categorizes_scoped_and_breaking_titles(self) -> None:
        sections, changelog = FORMATTER.parse_notes(
            "\n".join(
                [
                    "* feat!: replace the output schema by @author in https://example.test/1",
                    "* fix(parser)!: reject ambiguous input by @author in https://example.test/2",
                    "* docs(readme): explain migration by @author in https://example.test/3",
                    "**Full Changelog**: https://example.test/compare/v1...v2",
                ]
            )
        )

        self.assertEqual(sections["What's New"], ["replace the output schema"])
        self.assertEqual(sections["Fixes"], ["reject ambiguous input"])
        self.assertEqual(sections["Documentation"], ["explain migration"])
        self.assertEqual(changelog, "https://example.test/compare/v1...v2")

    def test_skips_internal_change_types(self) -> None:
        sections, _ = FORMATTER.parse_notes(
            "* ci: update workflow by @author in https://example.test/1\n"
            "* chore(deps): update lockfile by @author in https://example.test/2"
        )

        self.assertEqual(sections, {})

    def test_uncategorized_entries_are_preserved(self) -> None:
        sections, _ = FORMATTER.parse_notes(
            "* security: harden boundaries by @author in https://example.test/1\n"
            "* a title without a conventional prefix by @author in https://example.test/2"
        )

        self.assertEqual(
            sections["Other"],
            ["harden boundaries", "a title without a conventional prefix"],
        )

    def test_formatting_is_stable(self) -> None:
        rendered = FORMATTER.format_markdown(
            "v1.2.3",
            {"Fixes": ["repair a thing"]},
            "https://example.test/compare/v1.2.2...v1.2.3",
        )

        self.assertEqual(
            rendered,
            "## midden v1.2.3\n\n"
            "### Fixes\n"
            "- repair a thing\n\n"
            "---\n"
            "**Full Changelog**: https://example.test/compare/v1.2.2...v1.2.3\n",
        )


if __name__ == "__main__":
    unittest.main()
