# frozen_string_literal: true

require_relative "test_helper"
require_relative "../ci-path-routing"
require "yaml"

class CiPathRoutingTest < Minitest::Test
  include WorkflowHelpers

  def test_workflow_executes_only_the_base_branch_router
    source = workflow("ci.yml")
    assert_includes source, 'git show "${BASE_SHA}:scripts/ci-path-routing.rb"'
    assert_includes source, 'ruby "${ROUTER}"'
    refute_includes source, "| ruby scripts/ci-path-routing.rb"
    assert_includes job(source, "generate-matrix"), "runs-on: ubuntu-24.04"
  end

  def test_missing_base_router_fails_closed_to_full_matrix
    script = workflow_run_block(workflow("ci.yml"), "Generate CI matrix")
    Dir.mktmpdir do |root|
      output = File.join(root, "output")
      capture_success("bash", "-euo", "pipefail", "-c", "git() { return 1; }\nruby() { echo 'unexpected Ruby execution' >&2; exit 99; }\n" + script, env: {
        "BASE_SHA" => "a" * 40, "EVENT_NAME" => "pull_request", "RUNNER_TEMP" => root, "GITHUB_OUTPUT" => output
      })
      decisions = File.readlines(output, chomp: true).to_h { |line| key, value = line.split("=", 2); [key, JSON.parse(value)] }
      assert_equal CiPathRouting::FULL_MATRIX, decisions["matrix"]
      %w[links zizmor coverage].each { |name| assert_equal true, decisions[name] }
    end
  end

  def select_router_ruby(version: "4.0.7", router: true)
    Dir.mktmpdir do |root|
      env = { "HOME" => root, "XDG_CONFIG_HOME" => root, "GIT_CONFIG_NOSYSTEM" => "1" }
      git = ->(*args) { capture_success("git", "-c", "user.name=ci", "-c", "user.email=ci@example.com", *args, chdir: root, env: env) }
      git.call("init", "--quiet")
      if router
        FileUtils.mkdir_p(File.join(root, "scripts"))
        File.write(File.join(root, "scripts/ci-path-routing.rb"), "trusted router\n")
        File.write(File.join(root, ".ruby-version"), version)
      end
      git.call("add", "-A")
      git.call("commit", "--allow-empty", "--quiet", "-m", "base")
      base = git.call("rev-parse", "HEAD").strip
      File.write(File.join(root, ".ruby-version"), "untrusted-head-runtime\n")
      output = File.join(root, "output")
      script = workflow_run_block(workflow("ci.yml"), "Select trusted router Ruby")
      stdout, stderr, status = capture("bash", "-euo", "pipefail", "-c", script, chdir: root,
        env: env.merge("BASE_SHA" => base, "GITHUB_OUTPUT" => output))
      [status, File.exist?(output) ? File.read(output) : "", stdout + stderr]
    end
  end

  def test_router_runtime_is_selected_only_from_trusted_base
    status, output, logs = select_router_ruby
    assert status.success?, logs
    assert_equal "version=4.0.7\n", output
    steps = YAML.safe_load(workflow("ci.yml")).fetch("jobs").fetch("generate-matrix").fetch("steps")
    selection = steps.find { |step| step["name"] == "Select trusted router Ruby" }
    setup = steps.find { |step| step["name"] == "Set up router Ruby" }
    assert_equal "github.event_name == 'pull_request'", selection["if"]
    assert_equal "steps.trusted-ruby.outputs.version != ''", setup["if"]
    assert_equal({ "ruby-version" => "${{ steps.trusted-ruby.outputs.version }}", "bundler" => "none", "bundler-cache" => false }, setup["with"])
    assert_operator steps.index(setup), :<, steps.index { |step| step["name"] == "Generate CI matrix" }
  end

  def test_router_runtime_setup_is_skipped_before_router_introduction
    status, output, logs = select_router_ruby(router: false)
    assert status.success?, logs
    assert_empty output
  end

  def test_invalid_trusted_runtime_cannot_use_the_head_runtime
    ["4.0.7\nversion=0.0.0\n", "ruby-head"].each do |version|
      status, output, = select_router_ruby(version: version)
      refute status.success?
      assert_empty output
    end
  end

  def test_invalid_base_sha_fails_before_reading_repository_content
    ["Select trusted router Ruby", "Generate CI matrix"].each do |step|
      ["", "HEAD"].each do |base|
        Dir.mktmpdir do |root|
          output = File.join(root, "output")
          accessed = File.join(root, "accessed")
          script = "git() { touch \"$ACCESSED\"; exit 99; }\n" + workflow_run_block(workflow("ci.yml"), step)
          stdout, _stderr, status = capture("bash", "-euo", "pipefail", "-c", script, env: {
            "BASE_SHA" => base, "EVENT_NAME" => "pull_request", "RUNNER_TEMP" => root,
            "GITHUB_OUTPUT" => output, "ACCESSED" => accessed
          })
          refute status.success?
          assert_includes stdout, "Pull request base SHA is invalid."
          refute_path_exists accessed
          refute_path_exists output
        end
      end
    end
  end

  def test_dependency_policy_jobs_use_the_same_cargo_deny_archive
    archives = %w[ci.yml cargo-deny.yml].map do |name|
      pins = workflow(name).scan(%r{CARGO_DENY_SHA256: "([a-f0-9]{64})"\n[\s\S]*?https://github\.com/EmbarkStudios/cargo-deny/releases/download/([0-9.]+)/cargo-deny-([0-9.]+)-x86_64-unknown-linux-musl\.tar\.gz})
      assert_equal 1, pins.length, "expected one cargo-deny pin in #{name}"
      assert_equal pins[0][1], pins[0][2], "release and archive versions must match"
      pins[0]
    end
    assert_equal archives[0], archives[1]
  end

  def test_toolchain_setup_failure_stops_before_advisory_reporting
    script = workflow_run_block(workflow("cargo-deny.yml"), "Install cargo-deny")
    Dir.mktmpdir do |root|
      %w[curl sha256sum tar cargo].each do |name|
        body = case name
        when "sha256sum" then "cat >/dev/null\n"
        when "cargo" then "echo \"$*\" >> \"$CARGO_CALLS\"\nexit \"$CARGO_STATUS\"\n"
        else "exit 0\n"
        end
        write_executable(File.join(root, name), "#!/bin/sh\n" + body)
      end
      calls = File.join(root, "cargo-calls")
      [0, 42].each do |exit_status|
        File.write(calls, "")
        _stdout, stderr, status = capture("/bin/bash", "-euo", "pipefail", "-c", script, chdir: root, env: {
          "PATH" => "#{root}:/usr/bin:/bin", "RUNNER_TEMP" => root,
          "GITHUB_PATH" => File.join(root, "github-path"), "CARGO_DENY_SHA256" => "a" * 64,
          "CARGO_CALLS" => calls, "CARGO_STATUS" => exit_status.to_s
        })
        assert_equal exit_status, status.exitstatus, stderr
        assert_equal "--version\n", File.read(calls)
      end
    end
  end

  def test_repository_cargo_config_runs_the_full_matrix
    %w[.cargo/config .cargo/config.toml].each do |path|
      result = CiPathRouting.route([path])
      assert_equal CiPathRouting::FULL_MATRIX, result["matrix"]
      assert result["coverage"]
    end
  end

  def test_documentation_only_enables_link_check
    assert_includes workflow("ci.yml"), 'args: "--config lychee.toml README.md SECURITY.md"'
    %w[README.md SECURITY.md].each do |path|
      result = CiPathRouting.route([path])
      assert_empty result["matrix"]
      assert result["links"]
      refute result["zizmor"]
    end
  end

  def test_release_workflow_runs_code_and_workflow_checks
    result = CiPathRouting.route([".github/workflows/release.yml"])
    assert_equal CiPathRouting::FULL_MATRIX, result["matrix"]
    assert result["zizmor"]
  end

  def test_codecov_upload_uses_oidc_without_a_standing_secret
    codecov = job(workflow("ci.yml"), "codecov")
    ["id-token: write", "python3 -I codecov-uploader/scripts/upload-codecov.py", "--coverage reports/lcov.info", "--junit reports/target/nextest/ci/junit.xml", "github.event.pull_request.base.sha || github.sha"].each { |text| assert_includes codecov, text }
    ["codecov/codecov-action", "CODECOV_TOKEN", "secrets.", "environment:"].each { |text| refute_includes codecov, text }
  end

  def test_non_rust_integration_fixture_runs_the_full_matrix
    result = CiPathRouting.route(["tests/fixtures/provider-state.json"])
    assert_equal CiPathRouting::FULL_MATRIX, result["matrix"]
    assert result["coverage"]
  end

  def test_toolchain_and_lint_config_variants_run_the_full_matrix
    %w[rust-toolchain .rustfmt.toml .clippy.toml].each do |path|
      assert_equal CiPathRouting::FULL_MATRIX, CiPathRouting.route([path])["matrix"]
    end
  end

  def test_ruby_tooling_and_dependencies_run_lint
    %w[Gemfile Gemfile.lock .ruby-version scripts/ci-path-routing.rb scripts/tests/ci_path_routing_test.rb scripts/test.rb].each do |path|
      assert_equal CiPathRouting::LINT_MATRIX, CiPathRouting.route([path])["matrix"], path
    end
  end

  def test_renaming_a_rust_source_away_runs_the_full_matrix
    command = workflow("ci.yml").lines.find { |line| line.strip.start_with?("git diff ") }.strip.delete_suffix("\\").strip
    changed = Dir.mktmpdir do |root|
      env = { "HOME" => root, "XDG_CONFIG_HOME" => root, "GIT_CONFIG_NOSYSTEM" => "1" }
      repo = File.join(root, "repo")
      FileUtils.mkdir_p(File.join(repo, "src"))
      File.write(File.join(repo, "src/lib.rs"), "pub fn f() {}\n")
      git = ->(*args) { capture_success("git", "-c", "user.name=ci", "-c", "user.email=ci@example.com", *args, chdir: repo, env: env) }
      git.call("init", "--quiet")
      git.call("add", "-A")
      git.call("commit", "--quiet", "-m", "base")
      base = git.call("rev-parse", "HEAD").strip
      git.call("mv", "src/lib.rs", "src/lib.rs.bak")
      git.call("commit", "--quiet", "-m", "rename")
      CiPathRouting.read_paths(capture_success("bash", "-c", command, chdir: repo, env: env.merge("BASE_SHA" => base)))
    end
    assert_equal CiPathRouting::FULL_MATRIX, CiPathRouting.route(changed)["matrix"]
  end

  def test_unrelated_path_does_not_schedule_checks
    assert_equal({ "matrix" => [], "links" => false, "zizmor" => false, "coverage" => false }, CiPathRouting.route(["assets/example.txt"]))
  end

  def test_cli_emits_boolean_and_matrix_outputs
    output = capture_success(RbConfig.ruby, File.join(ROOT, "scripts/ci-path-routing.rb"), stdin_data: "README.md\0")
    assert_equal "matrix=[]\nlinks=true\nzizmor=false\ncoverage=false\n", output
  end

  def test_git_diff_preserves_non_ascii_and_control_character_names
    paths = ["src/bin/évil.rs", "src/bin/new\nline.rs", "src/bin/tab\tname.rs", "src/bin/quote\"name.rs", "tests/space at end "]
    command = workflow("ci.yml").lines.find { |line| line.strip.start_with?("git diff ") }.strip.delete_suffix("\\").strip
    changed = Dir.mktmpdir do |root|
      env = { "HOME" => root, "XDG_CONFIG_HOME" => root, "GIT_CONFIG_NOSYSTEM" => "1", "LANG" => nil, "LC_ALL" => "C" }
      git = ->(*args) { capture_success("git", "-c", "user.name=ci", "-c", "user.email=ci@example.com", *args, chdir: root, env: env) }
      git.call("init", "--quiet")
      git.call("commit", "--allow-empty", "--quiet", "-m", "base")
      base = git.call("rev-parse", "HEAD").strip
      paths.each do |path|
        FileUtils.mkdir_p(File.dirname(File.join(root, path)))
        File.write(File.join(root, path), "fixture\n")
      end
      git.call("add", "-A")
      git.call("commit", "--quiet", "-m", "add paths")
      raw = capture_success("bash", "-c", command, chdir: root, env: env.merge("BASE_SHA" => base))
      CiPathRouting.read_paths(raw)
    end
    assert_equal paths.map(&:b).sort, changed.sort
    changed.each do |path|
      output = capture_success(RbConfig.ruby, File.join(ROOT, "scripts/ci-path-routing.rb"),
        stdin_data: path + "\0", env: { "LANG" => nil, "LC_ALL" => "C" })
      matrix = JSON.parse(output.lines.first.split("=", 2).last)
      assert_equal CiPathRouting::FULL_MATRIX, matrix, path.inspect
    end
  end

  def test_raw_paths_accept_non_utf8_bytes_and_reject_incomplete_records
    paths = CiPathRouting.read_paths("src/bin/\xff.rs\0".b)
    assert_equal CiPathRouting::FULL_MATRIX, CiPathRouting.route(paths)["matrix"]
    assert_raises(ArgumentError) { CiPathRouting.read_paths("src/bin/évil.rs") }
    assert_empty CiPathRouting.read_paths("")
  end
end
