import importlib.util
import os
import re
import subprocess
import tempfile
import textwrap
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

    def test_dependency_policy_jobs_use_the_same_cargo_deny_archive(self) -> None:
        archives = []
        for name in ("ci.yml", "cargo-deny.yml"):
            workflow = WORKFLOW.with_name(name).read_text()
            pins = re.findall(
                r'CARGO_DENY_SHA256: "([a-f0-9]{64})"\n'
                r"[\s\S]*?https://github\.com/EmbarkStudios/cargo-deny/releases/"
                r"download/([0-9.]+)/cargo-deny-([0-9.]+)-x86_64-unknown-linux-musl\.tar\.gz",
                workflow,
            )
            self.assertEqual(len(pins), 1, f"expected one cargo-deny pin in {name}")
            self.assertEqual(pins[0][1], pins[0][2], "release and archive versions must match")
            archives.append(pins[0])
        self.assertEqual(archives[0], archives[1])

    def test_toolchain_setup_failure_stops_before_advisory_reporting(self) -> None:
        workflow = WORKFLOW.with_name("cargo-deny.yml").read_text()
        step = re.search(
            r"^      - name: Install cargo-deny\n(?:^        .*\n)*?"
            r"^        run: \|\n(?P<run>(?:^          .*\n|^\n)+)",
            workflow,
            re.MULTILINE,
        )
        self.assertIsNotNone(step)
        script = textwrap.dedent(step.group("run"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("curl", "sha256sum", "tar", "cargo"):
                stub = root / name
                body = "exit 0\n"
                if name == "sha256sum":
                    body = "cat >/dev/null\n"
                elif name == "cargo":
                    body = 'echo "$*" >> "$CARGO_CALLS"\nexit "$CARGO_STATUS"\n'
                stub.write_text("#!/bin/sh\n" + body)
                stub.chmod(0o755)
            calls = root / "cargo-calls"
            for status in (0, 42):
                with self.subTest(toolchain_exit=status):
                    calls.write_text("")
                    result = subprocess.run(
                        ["/bin/bash", "-euo", "pipefail", "-c", script],
                        cwd=root,
                        env={
                            **os.environ,
                            "PATH": f"{root}:/usr/bin:/bin",
                            "RUNNER_TEMP": directory,
                            "GITHUB_PATH": str(root / "github-path"),
                            "CARGO_DENY_SHA256": "a" * 64,
                            "CARGO_CALLS": str(calls),
                            "CARGO_STATUS": str(status),
                        },
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode, status, result.stderr)
                    self.assertEqual(calls.read_text(), "--version\n")

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
        self.assertIn("python3 -I codecov-uploader/scripts/upload-codecov.py", codecov_job)
        self.assertIn("--coverage reports/lcov.info", codecov_job)
        self.assertIn("--junit reports/target/nextest/ci/junit.xml", codecov_job)
        self.assertIn("github.event.pull_request.base.sha || github.sha", codecov_job)
        self.assertNotIn("codecov/codecov-action", codecov_job)
        self.assertNotIn("CODECOV_TOKEN", codecov_job)
        self.assertNotIn("secrets.", codecov_job)
        self.assertNotIn("environment:", codecov_job)

    def test_non_rust_integration_fixture_runs_the_full_matrix(self) -> None:
        result = ROUTING.route(["tests/fixtures/provider-state.json"])
        self.assertEqual(result["matrix"], ROUTING.FULL_MATRIX)
        self.assertTrue(result["coverage"])

    def test_toolchain_and_lint_config_variants_run_the_full_matrix(self) -> None:
        for path in ("rust-toolchain", ".rustfmt.toml", ".clippy.toml"):
            with self.subTest(path=path):
                self.assertEqual(ROUTING.route([path])["matrix"], ROUTING.FULL_MATRIX)

    def test_renaming_a_rust_source_away_runs_the_full_matrix(self) -> None:
        command = next(
            line.strip().removesuffix("\\").strip()
            for line in WORKFLOW.read_text().splitlines()
            if line.strip().startswith("git diff ")
        )
        with tempfile.TemporaryDirectory() as directory:
            env = {
                **os.environ,
                "HOME": directory,
                "XDG_CONFIG_HOME": directory,
                "GIT_CONFIG_NOSYSTEM": "1",
            }
            repo = Path(directory) / "repo"
            (repo / "src").mkdir(parents=True)
            (repo / "src/lib.rs").write_text("pub fn f() {}\n")

            def git(*args: str) -> str:
                return subprocess.run(
                    ["git", "-c", "user.name=ci", "-c", "user.email=ci@example.com", *args],
                    cwd=repo,
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout

            git("init", "--quiet")
            git("add", "-A")
            git("commit", "--quiet", "-m", "base")
            base = git("rev-parse", "HEAD").strip()
            git("mv", "src/lib.rs", "src/lib.rs.bak")
            git("commit", "--quiet", "-m", "rename")
            changed = subprocess.run(
                ["bash", "-c", command],
                cwd=repo,
                env={**env, "BASE_SHA": base},
                check=True,
                capture_output=True,
                text=True,
            ).stdout.splitlines()

        self.assertEqual(ROUTING.route(changed)["matrix"], ROUTING.FULL_MATRIX)

    def test_unrelated_path_does_not_schedule_checks(self) -> None:
        result = ROUTING.route(["assets/example.txt"])
        self.assertEqual(
            result,
            {"matrix": [], "links": False, "zizmor": False, "coverage": False},
        )


if __name__ == "__main__":
    unittest.main()
