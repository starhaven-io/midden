import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).parents[2] / ".github/workflows/cargo-deny.yml"
TITLE = "cargo deny check is failing on main"


class CargoDenyIssueTests(unittest.TestCase):
    def run_step(self, name, issues, list_status=0):
        workflow = WORKFLOW.read_text()
        shell = re.search(r"(?m)^defaults:\n  run:\n    shell: ([^\n]+)$", workflow)[1]
        self.assertNotIn("\n    defaults:", workflow, "job shell overrides need explicit test support")
        step = workflow.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
        step_shell = re.search(r"(?m)^        shell: ([^\n]+)$", step)
        if step_shell:
            shell = step_shell[1]
        script = textwrap.dedent(
            re.search(r"        run: \|\n((?:          .*\n|\n)+)", step)[1]
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "issues.json").write_text(json.dumps(issues))
            (root / "deny-output.txt").write_text("fixture advisory\n")
            shim = root / "gh"
            shim.write_text(
                "#!/usr/bin/env bash\nset -euo pipefail\n"
                'if [[ "$1 $2" == "issue list" ]]; then\n'
                '  [[ "$LIST_STATUS" == 0 ]] || exit "$LIST_STATUS"\n'
                '  while [[ "$1" != --jq ]]; do shift; done\n'
                '  jq -r "$2" issues.json\n'
                "else\n"
                '  printf "%s\\n" "$*" >> calls.txt\n'
                "fi\n"
            )
            shim.chmod(0o755)
            script_path = root / "step.sh"
            script_path.write_text(script)
            result = subprocess.run(
                [argument.replace("{0}", str(script_path)) for argument in shlex.split(shell)],
                cwd=root,
                env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                     "RUN_URL": "https://example.test/run/123",
                     "LIST_STATUS": str(list_status)},
                capture_output=True, text=True,
            )
            calls = (root / "calls.txt").read_text() if (root / "calls.txt").exists() else ""
            return result, calls

    def test_updates_and_closes_only_the_actions_app_issue(self):
        issues = [
            {"number": 1, "title": TITLE, "author": {"login": "person"}},
            {"number": 2, "title": TITLE, "author": {"login": "app/github-actions"}},
            {"number": 3, "title": "different", "author": {"login": "app/github-actions"}},
        ]
        result, calls = self.run_step("Open or update tracking issue", issues)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, "issue comment 2 --body-file issue-body.md\n")
        result, calls = self.run_step("Resolve tracking issue", issues)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("issue comment 2 --body ", calls)
        self.assertIn("issue close 2\n", calls)
        self.assertNotIn("issue close 1", calls)
        self.assertNotIn("issue close 3", calls)

    def test_creates_when_no_bot_issue_exists(self):
        result, calls = self.run_step("Open or update tracking issue", [])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, f"issue create --title {TITLE} --body-file issue-body.md\n")

    def test_list_failure_is_not_a_clean_recovery(self):
        for name in ("Open or update tracking issue", "Resolve tracking issue"):
            with self.subTest(step=name):
                result, calls = self.run_step(name, [], list_status=17)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, "")
