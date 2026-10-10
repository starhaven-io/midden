# frozen_string_literal: true

require_relative "test_helper"

class CheckTest < Minitest::Test
  include WorkflowHelpers

  def check(failing: "", missing: nil)
    Dir.mktmpdir do |root|
      %w[cargo bundle typos cargo-deny zizmor lychee].each do |tool|
        next if tool == missing

        write_executable(File.join(root, tool), <<~SH)
          #!/bin/sh
          printf '%s %s\\n' '#{tool}' "$*" >> "$CALLS"
          if [ '#{tool}' = "$FAILING_TOOL" ]; then exit 7; fi
        SH
      end
      calls = File.join(root, "calls")
      stdout, stderr, status = capture(RbConfig.ruby, File.join(ROOT, "scripts/check.rb"), env: {
        "PATH" => root, "CALLS" => calls, "FAILING_TOOL" => failing
      })
      [status, stdout, stderr, File.read(calls)]
    end
  end

  def test_runs_all_required_checks
    status, _stdout, stderr, calls = check
    assert status.success?, stderr
    assert_equal [
      "cargo clippy --locked --all-targets -- -D warnings", "cargo fmt -- --check",
      "bundle exec ruby scripts/test.rb", "typos ", "cargo deny check",
      "zizmor --strict-collection --persona auditor .github/workflows/",
      "lychee --config lychee.toml README.md SECURITY.md", "cargo test --locked"
    ], calls.lines.map(&:chomp)
  end

  def test_failure_does_not_skip_later_checks
    status, _stdout, _stderr, calls = check(failing: "bundle")
    refute status.success?
    assert_includes calls, "cargo test --locked\n"
  end

  def test_missing_optional_tool_fails_the_gate
    status, stdout, _stderr, calls = check(missing: "typos")
    refute status.success?
    assert_includes stdout, "typos (brew install typos-cli)"
    assert_includes calls, "cargo test --locked\n"
  end

  def test_missing_required_commands_are_named
    %w[bundle cargo].each do |tool|
      status, _stdout, stderr, calls = check(missing: tool)
      refute status.success?
      assert_includes stderr, "#{tool}: command not found"
      assert_includes calls, "lychee --config lychee.toml README.md SECURITY.md"
    end
  end
end
