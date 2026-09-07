import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "ci-path-routing.py"
WORKFLOW = Path(__file__).parents[2] / ".github/workflows/ci.yml"
SPEC = importlib.util.spec_from_file_location("ci_path_routing", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
ROUTING = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ROUTING)


class CiPathRoutingTests(unittest.TestCase):
    def test_workflow_executes_only_the_base_branch_router(self) -> None:
        workflow = WORKFLOW.read_text()
        self.assertIn(
            'git show "${BASE_SHA}:scripts/ci-path-routing.py"', workflow
        )
        self.assertIn('python3 "${ROUTER}"', workflow)
        self.assertNotIn(
            "| python3 scripts/ci-path-routing.py", workflow
        )

    def test_repository_cargo_config_runs_the_full_matrix(self) -> None:
        for path in (".cargo/config", ".cargo/config.toml"):
            with self.subTest(path=path):
                result = ROUTING.route([path])
                self.assertEqual(result["matrix"], ROUTING.FULL_MATRIX)
                self.assertTrue(result["coverage"])

    def test_documentation_only_enables_link_check(self) -> None:
        workflow = WORKFLOW.read_text()
        self.assertIn('args: "--config lychee.toml README.md SECURITY.md"', workflow)
        for path in ("README.md", "SECURITY.md"):
            with self.subTest(path=path):
                result = ROUTING.route([path])
                self.assertEqual(result["matrix"], [])
                self.assertTrue(result["links"])
                self.assertFalse(result["zizmor"])

    def test_release_workflow_runs_code_and_workflow_checks(self) -> None:
        result = ROUTING.route([".github/workflows/release.yml"])
        self.assertEqual(result["matrix"], ROUTING.FULL_MATRIX)
        self.assertTrue(result["zizmor"])

    def test_codecov_upload_uses_oidc_without_a_standing_secret(self) -> None:
        workflow = WORKFLOW.read_text()
        codecov_job = workflow.split("  codecov:\n", 1)[1].split(
            "\n  links:\n", 1
        )[0]

        self.assertIn("id-token: write", codecov_job)
        self.assertEqual(codecov_job.count("use_oidc: true"), 2)
        self.assertNotIn("CODECOV_TOKEN", codecov_job)
        self.assertNotIn("secrets.", codecov_job)
        self.assertNotIn("environment:", codecov_job)

    def test_non_rust_integration_fixture_runs_the_full_matrix(self) -> None:
        result = ROUTING.route(["tests/fixtures/provider-state.json"])
        self.assertEqual(result["matrix"], ROUTING.FULL_MATRIX)
        self.assertTrue(result["coverage"])

    def test_unrelated_path_does_not_schedule_checks(self) -> None:
        result = ROUTING.route(["assets/example.txt"])
        self.assertEqual(
            result,
            {"matrix": [], "links": False, "zizmor": False, "coverage": False},
        )


if __name__ == "__main__":
    unittest.main()
