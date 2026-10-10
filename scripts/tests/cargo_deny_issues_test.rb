# frozen_string_literal: true

require_relative "test_helper"

class CargoDenyIssueTest < Minitest::Test
  include WorkflowHelpers
  TITLE = "cargo deny check is failing on main"

  def run_step(name, issues, list_status: 0)
    source = workflow("cargo-deny.yml")
    shell = source.match(/^defaults:\n  run:\n    shell: ([^\n]+)$/)[1]
    refute_includes source, "\n    defaults:", "job shell overrides need explicit test support"
    step = source.split("      - name: #{name}\n", 2).fetch(1).split("\n      - ", 2).first
    shell = step.match(/^        shell: ([^\n]+)$/)&.[](1) || shell
    script = run_blocks(step).first
    Dir.mktmpdir do |root|
      File.write(File.join(root, "issues.json"), JSON.generate(issues))
      File.write(File.join(root, "deny-output.txt"), "fixture advisory\n")
      write_executable(File.join(root, "gh"), <<~'SH')
        #!/usr/bin/env bash
        set -euo pipefail
        if [[ "$1 $2" == "issue list" ]]; then
          [[ "$LIST_STATUS" == 0 ]] || exit "$LIST_STATUS"
          while [[ "$1" != --jq ]]; do shift; done
          jq -r "$2" issues.json
        else
          printf "%s\n" "$*" >> calls.txt
        fi
      SH
      script_path = File.join(root, "step.sh")
      File.write(script_path, script)
      stdout, stderr, status = capture(*Shellwords.split(shell).map { |argument| argument.gsub("{0}", script_path) },
        chdir: root, env: { "PATH" => "#{root}:#{ENV.fetch('PATH')}", "RUN_URL" => "https://example.test/run/123", "LIST_STATUS" => list_status.to_s })
      calls = File.join(root, "calls.txt")
      [status, File.exist?(calls) ? File.read(calls) : "", stderr, stdout]
    end
  end

  def test_updates_and_closes_only_the_actions_app_issue
    issues = [
      { "number" => 1, "title" => TITLE, "author" => { "login" => "person" } },
      { "number" => 2, "title" => TITLE, "author" => { "login" => "app/github-actions" } },
      { "number" => 3, "title" => "different", "author" => { "login" => "app/github-actions" } }
    ]
    status, calls, stderr = run_step("Open or update tracking issue", issues)
    assert status.success?, stderr
    assert_equal "issue comment 2 --body-file issue-body.md\n", calls
    status, calls, stderr = run_step("Resolve tracking issue", issues)
    assert status.success?, stderr
    assert_includes calls, "issue comment 2 --body "
    assert_includes calls, "issue close 2\n"
    refute_includes calls, "issue close 1"
    refute_includes calls, "issue close 3"
  end

  def test_creates_when_no_bot_issue_exists
    status, calls, stderr = run_step("Open or update tracking issue", [])
    assert status.success?, stderr
    assert_equal "issue create --title #{TITLE} --body-file issue-body.md\n", calls
  end

  def test_list_failure_is_not_a_clean_recovery
    ["Open or update tracking issue", "Resolve tracking issue"].each do |name|
      status, calls = run_step(name, [], list_status: 17)
      refute status.success?
      assert_empty calls
    end
  end
end
