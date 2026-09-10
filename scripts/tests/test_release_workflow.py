import json
import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


WORKFLOW = Path(__file__).parents[2] / ".github/workflows/release.yml"


def job(source: str, name: str) -> str:
    headings = list(re.finditer(r"(?m)^  ([a-zA-Z0-9_-]+):\n", source))
    for index, heading in enumerate(headings):
        if heading.group(1) == name:
            end = headings[index + 1].start() if index + 1 < len(headings) else len(source)
            return source[heading.start() : end]
    raise AssertionError(f"job not found: {name}")


def run_blocks(source: str) -> list[str]:
    lines = source.splitlines()
    blocks = []
    index = 0
    while index < len(lines):
        match = re.match(r"^(\s*)run:\s*\|\s*$", lines[index])
        if match is None:
            index += 1
            continue
        indentation = len(match.group(1))
        block = []
        index += 1
        while index < len(lines):
            line = lines[index]
            leading = len(line) - len(line.lstrip())
            if line.strip() and leading <= indentation:
                break
            block.append(line)
            index += 1
        blocks.append("\n".join(block))
    return blocks


class ReleaseWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = WORKFLOW.read_text()

    def test_distinct_release_requests_are_retained(self) -> None:
        concurrency = self.source.split("concurrency:\n", 1)[1].split("\n\n", 1)[0]
        self.assertEqual(dict(line.strip().split(": ", 1) for line in concurrency.splitlines()), {
            "group": "release", "cancel-in-progress": "false", "queue": "max",
        })

    def test_standalone_notarization_uses_designated_requirement(self) -> None:
        signing = job(self.source, "sign-macos")
        self.assertIn("jq -e '.status == \"Accepted\"'", signing)
        self.assertIn("-R='notarized' --check-notarization", signing)
        self.assertNotIn("spctl --assess", signing)

    def test_existing_release_assets_are_immutable(self) -> None:
        publication = job(self.source, "release")
        self.assertIn("cmp --silent", publication)
        self.assertIn("existing release asset", publication)
        self.assertNotIn("--clobber", publication)

    def test_api_status_preserves_http_and_transport_contracts(self) -> None:
        helpers = [
            re.search(r"(?ms)^api_status\(\) \{\n.*?^\}", textwrap.dedent(block)).group()
            for block in run_blocks(self.source) if "api_status()" in block
        ]
        self.assertEqual(len(helpers), 2)
        curl = r'''#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
assert args[:5] == ["--silent", "--show-error", "--retry", "3", "--header"]
assert "--fail" not in args
assert args[args.index("--write-out") + 1] == "%{http_code}"
assert args[-1] == "https://api.github.test/resource"
Path(args[args.index("--output") + 1]).write_text(os.environ["RESPONSE_BODY"])
print(os.environ["HTTP_STATUS"], end="")
sys.exit(int(os.environ["CURL_STATUS"]))
'''
        for helper_index, helper in enumerate(helpers):
            for target in ("tag.json", "tag-commit.json", "release.json", "branch-ref.json"):
                for http_status, curl_status in (("200", 0), ("404", 0), ("500", 0), ("000", 7), ("200", 18)):
                    with self.subTest(helper=helper_index, target=target, http=http_status, curl=curl_status):
                        with tempfile.TemporaryDirectory() as directory:
                            path = Path(directory)
                            (path / "curl").write_text(curl)
                            (path / "curl").chmod(0o755)
                            body = json.dumps({"status": http_status, "target": target})
                            environment = {
                                **os.environ, "PATH": f"{path}:{os.environ['PATH']}",
                                "GH_TOKEN": "fixture-token", "HTTP_STATUS": http_status,
                                "CURL_STATUS": str(curl_status), "RESPONSE_BODY": body,
                            }
                            invocation = f'api_status https://api.github.test/resource {target}'
                            result = subprocess.run(
                                ["bash", "-euo", "pipefail", "-c", helper + "\n" + invocation],
                                cwd=path, env=environment, capture_output=True, text=True,
                            )
                            self.assertEqual(result.returncode, curl_status, result.stderr)
                            self.assertEqual(result.stdout, http_status)
                            if curl_status == 0:
                                self.assertEqual((path / target).read_text(), body)
                                self.assertFalse((path / "api-response.json").exists())
                            else:
                                self.assertFalse((path / target).exists())
                                caller = subprocess.run(
                                    ["bash", "-euo", "pipefail", "-c",
                                     helper + f'\nSTATUS=$({invocation})\nprintf continued'],
                                    cwd=path, env=environment, capture_output=True, text=True,
                                )
                                self.assertEqual(caller.returncode, curl_status)
                                self.assertEqual(caller.stdout, "")

    def test_linux_build_has_no_attestation_authority(self) -> None:
        build = job(self.source, "build-linux")
        attest = job(self.source, "attest-linux")
        self.assertNotIn("id-token: write", build)
        self.assertNotIn("attestations: write", build)
        self.assertIn("id-token: write", attest)
        self.assertIn("attestations: write", attest)
        self.assertNotIn("actions/checkout", attest)

    def test_shell_functions_are_defined_in_every_run_block_that_calls_them(self) -> None:
        blocks = run_blocks(self.source)
        definitions = [
            set(re.findall(r"(?m)^\s*([a-zA-Z_][a-zA-Z0-9_]*)\(\)\s*\{", block))
            for block in blocks
        ]
        known_functions = set().union(*definitions)

        for index, (block, local_definitions) in enumerate(zip(blocks, definitions)):
            for name in known_functions - local_definitions:
                invocation = re.compile(
                    rf"\$\(\s*{re.escape(name)}(?:\s|\))|"
                    rf"^\s*{re.escape(name)}(?:\s|$)",
                    re.MULTILINE,
                )
                self.assertIsNone(
                    invocation.search(block),
                    f"run block {index} calls shell function {name!r} defined only elsewhere",
                )

    def test_generated_cask_branch_and_commit_match_publisher_policy(self) -> None:
        preparation = job(self.source, "prepare-cask-bump")
        branch_line = re.search(r'(?m)^\s*BRANCH="[^"\n]+"$', preparation)
        self.assertIsNotNone(branch_line)
        write = job(self.source, "write-cask-bump")
        block = next(block for block in run_blocks(write) if "COMMIT_SHA=$(jq" in block)
        # Execute the actual payload builder with a local API fixture. No network or ref writes.
        builder = textwrap.dedent(block).split("REF_STATUS=", 1)[0]
        stub = r"""
        # The job uses GNU base64; normalize the fixture on macOS without changing job code.
        base64() {
          test "$1" = -w && test "$2" = 0
          command base64 < "$3" | tr -d '\n'
        }
        gh() {
          case "$*" in
            'api users/starhaven-bot[bot] --jq .id') printf '12345' ;;
            'api repos/starhaven-io/homebrew-tap --jq .default_branch') printf 'main' ;;
            'api repos/starhaven-io/homebrew-tap/git/ref/heads/main --jq .object.sha') printf 'base' ;;
            'api repos/starhaven-io/homebrew-tap/git/commits/base --jq .tree.sha') printf 'base-tree' ;;
            *'/git/blobs --input - --jq .sha') cat > blob.json; printf 'blob' ;;
            *'/git/trees --input - --jq .sha') cat > tree.json; printf 'tree' ;;
            *'/git/commits --input - --jq .sha') cat > commit.json; printf 'commit' ;;
            *) printf 'Unexpected fixture API call: %s\n' "$*" >&2; return 1 ;;
          esac
        }
        """
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "cask-plan").mkdir()
            (path / "cask-plan/candidate-cask.rb").write_text('cask "midden" do\nend\n')
            environment = {
                **os.environ, "APP_SLUG": "starhaven-bot", "VERSION": "1.2.3", "BASE_SHA": "base",
                "BASE_BRANCH": "main",
            }
            subprocess.run(["bash", "-euo", "pipefail", "-c", stub + builder], cwd=path, env=environment, check=True)
            commit = json.loads((path / "commit.json").read_text())
            branch = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", branch_line.group() + '\nprintf "%s" "$BRANCH"'],
                env=environment, check=True, capture_output=True, text=True,
            ).stdout
        self.assertEqual(branch, "bump-midden-1.2.3")
        self.assertEqual(commit["tree"], "tree")
        self.assertEqual(commit["parents"], ["base"])
        self.assertEqual(commit["author"], {
            "name": "starhaven-bot[bot]", "email": "12345+starhaven-bot[bot]@users.noreply.github.com",
        })
        self.assertEqual(commit["committer"], commit["author"])
        trailers = subprocess.run(
            ["git", "interpret-trailers", "--parse"], input=commit["message"], text=True,
            check=True, capture_output=True,
        ).stdout
        self.assertEqual(trailers.strip(), f'Signed-off-by: {commit["author"]["name"]} <{commit["author"]["email"]}>')
        self.assertIn("and .author.name == $name and .author.email == $email", write)
        self.assertIn("and .committer.name == $name and .committer.email == $email", write)

    def test_cask_check_wait_does_not_accept_partial_registration(self) -> None:
        merge = job(self.source, "merge-cask-bump")
        wait = next(block for block in run_blocks(merge) if "CHECK_TIMEOUT_SECONDS" in block)
        wait = textwrap.dedent(wait).replace("CHECK_INTERVAL_SECONDS=10", "CHECK_INTERVAL_SECONDS=0")
        stub = r'''
        gh() {
          if [[ "$1" == api && "$2" == "/repos/starhaven-io/homebrew-tap/pulls/${PR_NUMBER}" ]]; then
            printf '%s\n' validated-head
            return
          fi
          if [[ "$1" == pr && "$2" == checks && "$*" == *--json* ]]; then
            printf '1\n'
            return
          fi
          if [[ "$1" == pr && "$2" == checks ]]; then
            index=$(< "${GH_FIXTURE_COUNTER}")
            if [[ "${index}" == 1 ]]; then
              printf 'conclusion pending\n'
              return 8
            fi
            printf 'visible required checks passed\n'
            return
          fi
          if [[ "$1" == pr && "$2" == view ]]; then
            index=$(< "${GH_FIXTURE_COUNTER}")
            printf '%s\n' "$((index + 1))" > "${GH_FIXTURE_COUNTER}"
            cat "${GH_FIXTURE_DIR}/${index}.json"
            return
          fi
          printf 'unexpected gh call: %s\n' "$*" >&2
          return 1
        }
        '''
        fixtures = [
            {"headRefOid": "validated-head", "mergeStateStatus": "BLOCKED"},
            {"headRefOid": "validated-head", "mergeStateStatus": "BLOCKED"},
            {"headRefOid": "validated-head", "mergeStateStatus": "CLEAN"},
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            counter = path / "counter"
            counter.write_text("0\n")
            for index, fixture in enumerate(fixtures):
                (path / f"{index}.json").write_text(json.dumps(fixture))
            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", textwrap.dedent(stub) + wait],
                env={
                    **os.environ,
                    "GH_FIXTURE_COUNTER": str(counter),
                    "GH_FIXTURE_DIR": str(path),
                    "PR_NUMBER": "159",
                    "HEAD_SHA": "validated-head",
                },
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertEqual(counter.read_text().strip(), "3")
        self.assertIn("Required cask checks: passing; merge state: BLOCKED", result.stdout)
        self.assertIn("Required cask checks: pending; merge state: BLOCKED", result.stdout)
        self.assertIn("Required cask checks: passing; merge state: CLEAN", result.stdout)

    def test_cask_validation_is_unprivileged_and_head_bound(self) -> None:
        preparation = job(self.source, "prepare-cask-bump")
        write = job(self.source, "write-cask-bump")
        validation = job(self.source, "validate-cask-bump")
        merge = job(self.source, "merge-cask-bump")
        fetch = preparation.split("- name: Fetch cask inputs", 1)[1].split(
            "- name: Render and validate exact cask candidate", 1
        )[0]
        render = preparation.split(
            "- name: Render and validate exact cask candidate", 1
        )[1].split("- name: Upload cask plan", 1)[0]
        self.assertIn("GH_TOKEN: ${{ github.token }}", fetch)
        # Cask inputs come from gh, not curl: an unpinned curl fetch of a
        # non-data URL is a pinprick shell_fetch finding on a fail-closed audit.
        self.assertNotIn("curl", fetch)
        self.assertIn("gh api", fetch)
        self.assertNotIn("validate-cask-bump.py", fetch)
        # Cask checksums come from the build artifact the publication job
        # already bound to the release, not from a re-download.
        self.assertIn("actions/download-artifact", preparation)
        self.assertNotIn("gh release download", preparation)
        self.assertNotIn("releases/download", preparation)
        self.assertIn("validate-cask-bump.py", render)
        self.assertNotIn("GH_TOKEN", render)
        self.assertIn("scripts/validate-cask-bump.py --render", preparation)
        self.assertIn("scripts/validate-cask-bump.py", preparation)
        self.assertIn("cmp --silent base-cask.rb candidate-cask.rb", preparation)
        self.assertNotIn("create-github-app-token", preparation)
        self.assertNotIn("Homebrew/actions", preparation)
        self.assertNotRegex(preparation, r"(?m)^\s+brew\s")
        self.assertNotIn("actions/checkout", write)
        self.assertNotIn("Homebrew/actions", write)
        self.assertNotRegex(write, r"(?m)^\s+brew\s")
        self.assertNotIn("scripts/", write)
        self.assertIn("-f state=all", write)
        self.assertIn(".tree.sha == $tree", write)
        self.assertIn("(.parents | length) == 1", write)
        self.assertIn('if [[ "${MATCH_COUNT}" == "0" ]]', write)
        self.assertNotIn("force=true", write)
        self.assertLess(
            write.index("Validate cask plan artifact"),
            write.index("Mint bot token for tap"),
        )
        self.assertNotIn("actions/checkout", validation)
        self.assertNotIn("tap-token.outputs.token", validation)
        self.assertNotIn("APP_PRIVATE_KEY", validation)
        self.assertNotIn("actions/checkout", merge)
        self.assertIn("gh pr checks", merge)
        self.assertIn("CHECK_STATUS=0", merge)
        self.assertIn("8) CHECK_SUMMARY=pending", merge)
        self.assertIn("mergeStateStatus", merge)
        self.assertIn("CHECK_STATUS == 0", merge)
        self.assertIn(
            '[[ "${MERGE_STATE}" == "CLEAN" || "${MERGE_STATE}" == "UNSTABLE" ]]',
            merge,
        )
        self.assertIn("CHECK_TIMEOUT_SECONDS=1500", merge)
        self.assertIn("no required checks appeared before the cask check timeout", merge)
        self.assertIn("did not satisfy branch policy before the timeout", merge)
        self.assertIn("a required cask check failed", merge)
        self.assertNotIn("--watch", merge)
        self.assertNotIn("--fail-fast", merge)
        self.assertNotIn("--auto", merge)
        self.assertIn('--match-head-commit "${HEAD_SHA}"', merge)


if __name__ == "__main__":
    unittest.main()
