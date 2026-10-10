# frozen_string_literal: true

require_relative "test_helper"
require "digest"
require "set"
require "yaml"

class ReleaseWorkflowTest < Minitest::Test
  include WorkflowHelpers

  def setup
    @source = workflow("release.yml")
  end

  def test_distinct_release_requests_are_retained
    concurrency = @source.split("concurrency:\n", 2).fetch(1).split("\n\n", 2).first
    assert_equal({ "group" => "release", "cancel-in-progress" => "false", "queue" => "max" }, concurrency.lines.to_h { |line| line.strip.split(": ", 2) })
  end

  def test_ruby_dependency_caches_are_confined_to_unprivileged_release_jobs
    cached_jobs = YAML.safe_load(@source).fetch("jobs").select do |_name, definition|
      definition.fetch("steps").any? do |step|
        step.fetch("uses", "").start_with?("ruby/setup-ruby@") && step.fetch("with", {})["bundler-cache"] == true
      end
    end
    assert_equal %w[prepare format-notes], cached_jobs.keys
    cached_jobs.each do |name, definition|
      assert_equal({ "contents" => "read" }, definition["permissions"], name)
      refute definition.key?("environment"), name
      source = job(@source, name)
      refute_includes source, "secrets."
      refute_includes source, "create-github-app-token"
    end
  end

  def test_cask_preparation_uses_stdlib_ruby_without_installing_dependencies
    definition = YAML.safe_load(@source).fetch("jobs").fetch("prepare-cask-bump")
    ruby_steps = definition.fetch("steps").select { |step| step.fetch("uses", "").start_with?("ruby/setup-ruby@") }
    assert_equal 1, ruby_steps.length
    assert_equal({ "bundler" => "none", "bundler-cache" => false }, ruby_steps.first.fetch("with"))
    assert_equal({ "contents" => "read" }, definition.fetch("permissions"))
    refute definition.key?("environment")
    preparation = job(@source, "prepare-cask-bump")
    refute_match(/\bbundle(?:r)?\s+(?:exec|install)\b/, preparation)
    assert_includes preparation, "ruby scripts/validate-cask-bump.rb --render"
  end

  def test_standalone_notarization_uses_designated_requirement
    signing = job(@source, "sign-macos")
    assert_includes signing, "jq -e '.status == \"Accepted\"'"
    assert_includes signing, "-R='notarized' --check-notarization"
    refute_includes signing, "spctl --assess"
  end

  def test_existing_release_assets_are_immutable
    publication = job(@source, "release")
    assert_includes publication, "cmp --silent"
    assert_includes publication, "existing release asset"
    refute_includes publication, "--clobber"
  end

  def test_tag_authority_is_limited_to_publication
    publication = job(@source, "release")
    token = publication.split("id: release-token\n", 2).fetch(1).split("\n      - name:", 2).first
    assert_equal({ "contents" => "write", "workflows" => "write" }, token.scan(/permission-([\w-]+): (\w+)/).to_h)
    assert_includes token, "repositories: ${{ github.event.repository.name }}"
    refute_includes publication, "actions/checkout"
    assert_equal 1, @source.scan("permission-workflows:").length
  end

  def test_api_status_preserves_http_and_transport_contracts
    helpers = run_blocks(@source).filter_map { |block| block.match(/^api_status\(\) \{\n.*?^\}/m)&.to_s }
    assert_equal 2, helpers.length
    curl = <<~'SCRIPT'
      #!/usr/bin/env ruby
      abort "unexpected arguments" unless ARGV.take(5) == ["--silent", "--show-error", "--retry", "3", "--header"]
      abort "unexpected --fail" if ARGV.include?("--fail")
      abort "unexpected format" unless ARGV[ARGV.index("--write-out") + 1] == "%{http_code}"
      abort "unexpected URL" unless ARGV.last == "https://api.github.test/resource"
      File.write(ARGV[ARGV.index("--output") + 1], ENV.fetch("RESPONSE_BODY"))
      print ENV.fetch("HTTP_STATUS")
      exit Integer(ENV.fetch("CURL_STATUS"))
    SCRIPT
    helpers.each do |helper|
      %w[tag.json tag-commit.json release.json branch-ref.json].each do |target|
        [["200", 0], ["404", 0], ["500", 0], ["000", 7], ["200", 18]].each do |http_status, curl_status|
          Dir.mktmpdir do |root|
            write_executable(File.join(root, "curl"), curl)
            body = JSON.generate("status" => http_status, "target" => target)
            env = { "PATH" => "#{root}:#{ENV.fetch('PATH')}", "GH_TOKEN" => "fixture-token", "HTTP_STATUS" => http_status, "CURL_STATUS" => curl_status.to_s, "RESPONSE_BODY" => body }
            invocation = "api_status https://api.github.test/resource #{target}"
            stdout, stderr, status = capture("bash", "-euo", "pipefail", "-c", helper + "\n" + invocation, chdir: root, env: env)
            assert_equal curl_status, status.exitstatus, stderr
            assert_equal http_status, stdout
            if curl_status.zero?
              assert_equal body, File.read(File.join(root, target))
              refute_path_exists File.join(root, "api-response.json")
            else
              refute_path_exists File.join(root, target)
              stdout, _stderr, status = capture("bash", "-euo", "pipefail", "-c", helper + "\nSTATUS=$(#{invocation})\nprintf continued", chdir: root, env: env)
              assert_equal curl_status, status.exitstatus
              assert_empty stdout
            end
          end
        end
      end
    end
  end

  def test_linux_build_has_no_attestation_authority
    build = job(@source, "build-linux")
    attest = job(@source, "attest-linux")
    refute_includes build, "id-token: write"
    refute_includes build, "attestations: write"
    assert_includes attest, "id-token: write"
    assert_includes attest, "attestations: write"
    refute_includes attest, "actions/checkout"
  end

  def test_shell_functions_are_defined_in_every_run_block_that_calls_them
    blocks = run_blocks(@source)
    definitions = blocks.map { |block| block.scan(/^\s*([a-zA-Z_][a-zA-Z0-9_]*)\(\)\s*\{/).flatten.to_set }
    known_functions = definitions.reduce(Set.new, :|)
    blocks.zip(definitions).each_with_index do |(block, local_definitions), index|
      (known_functions - local_definitions).each do |name|
        invocation = /\$\(\s*#{Regexp.escape(name)}(?:\s|\))|^\s*#{Regexp.escape(name)}(?:\s|$)/
        refute_match invocation, block, "run block #{index} calls shell function #{name.inspect} defined only elsewhere"
      end
    end
  end

  def test_generated_cask_branch_and_commit_match_publisher_policy
    preparation = job(@source, "prepare-cask-bump")
    branch_line = preparation.match(/^\s*BRANCH="[^"\n]+"$/)
    refute_nil branch_line
    write = job(@source, "write-cask-bump")
    builder = run_blocks(write).find { |block| block.include?("COMMIT_SHA=$(jq") }.split("REF_STATUS=", 2).first
    # The job uses GNU base64; normalize the fixture on macOS without changing job code.
    stub = <<~'SH'
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
    SH
    commit, branch = Dir.mktmpdir do |root|
      FileUtils.mkdir_p(File.join(root, "cask-plan"))
      File.write(File.join(root, "cask-plan/candidate-cask.rb"), "cask \"midden\" do\nend\n")
      env = { "APP_SLUG" => "starhaven-bot", "VERSION" => "1.2.3", "BASE_SHA" => "base", "BASE_BRANCH" => "main" }
      capture_success("bash", "-euo", "pipefail", "-c", stub + builder, chdir: root, env: env)
      branch = capture_success("bash", "-euo", "pipefail", "-c", branch_line.to_s + "\n" + 'printf "%s" "$BRANCH"', env: env)
      [JSON.parse(File.read(File.join(root, "commit.json"))), branch]
    end
    assert_equal "bump-midden-1.2.3", branch
    assert_equal "tree", commit["tree"]
    assert_equal ["base"], commit["parents"]
    assert_equal({ "name" => "starhaven-bot[bot]", "email" => "12345+starhaven-bot[bot]@users.noreply.github.com" }, commit["author"])
    assert_equal commit["author"], commit["committer"]
    trailers = capture_success("git", "interpret-trailers", "--parse", stdin_data: commit["message"])
    assert_equal "Signed-off-by: #{commit['author']['name']} <#{commit['author']['email']}>", trailers.strip
    assert_includes write, "and .author.name == $name and .author.email == $email"
    assert_includes write, "and .committer.name == $name and .committer.email == $email"
  end

  def test_cask_writer_checks_the_exact_regular_file_before_minting_credentials
    steps = YAML.safe_load(@source).fetch("jobs").fetch("write-cask-bump").fetch("steps")
    validation = steps.find { |step| step["name"] == "Validate cask plan artifact" }
    token = steps.find { |step| step["name"] == "Mint bot token for tap" }
    assert_operator steps.index(validation), :<, steps.index(token)
    assert_equal({ "EXPECTED_SHA256" => "${{ needs.prepare-cask-bump.outputs.candidate_sha256 }}" }, validation.fetch("env"))
    refute validation.key?("if")
    refute validation.fetch("continue-on-error", false)
    content = "cask \"midden\" do\n  version \"1.2.3\"\nend\n"
    digest = Digest::SHA256.hexdigest(content)
    %i[valid empty_digest wrong_digest symlink broken_symlink missing directory extra_file extra_directory].each do |fixture|
      Dir.mktmpdir do |root|
        FileUtils.mkdir_p(File.join(root, "cask-plan"))
        candidate = File.join(root, "cask-plan/candidate-cask.rb")
        case fixture
        when :symlink, :broken_symlink
          target = File.join(root, "outside.rb")
          File.write(target, content) if fixture == :symlink
          File.symlink(target, candidate)
        when :directory
          FileUtils.mkdir_p(candidate)
        else
          File.write(candidate, content) unless fixture == :missing
        end
        File.write(File.join(root, "cask-plan/extra.rb"), content) if fixture == :extra_file
        FileUtils.mkdir_p(File.join(root, "cask-plan/extra")) if fixture == :extra_directory
        expected_digest = { empty_digest: "", wrong_digest: "0" * 64 }.fetch(fixture, digest)
        # Provide GNU sha256sum's output without making it a macOS prerequisite.
        write_executable(File.join(root, "sha256sum"), <<~RUBY)
          #!#{RbConfig.ruby}
          require "digest"
          abort "unexpected hash input" unless ARGV == ["cask-plan/candidate-cask.rb"]
          puts "\#{Digest::SHA256.file(ARGV.first).hexdigest}  \#{ARGV.first}"
        RUBY
        _stdout, stderr, status = capture("bash", "-euo", "pipefail", "-c", validation.fetch("run"), chdir: root,
          env: { "PATH" => "#{root}:#{ENV.fetch('PATH')}", "EXPECTED_SHA256" => expected_digest })
        assert_equal fixture == :valid, status.success?, "#{fixture}: #{stderr}"
      end
    end
  end

  def test_cask_check_wait_does_not_accept_partial_registration
    wait = run_blocks(job(@source, "merge-cask-bump")).find { |block| block.include?("CHECK_TIMEOUT_SECONDS") }.gsub("CHECK_INTERVAL_SECONDS=10", "CHECK_INTERVAL_SECONDS=0")
    stub = <<~'SH'
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
    SH
    stdout = Dir.mktmpdir do |root|
      counter = File.join(root, "counter")
      File.write(counter, "0\n")
      %w[BLOCKED BLOCKED CLEAN].each_with_index do |state, index|
        File.write(File.join(root, "#{index}.json"), JSON.generate("headRefOid" => "validated-head", "mergeStateStatus" => state))
      end
      result = capture_success("bash", "-euo", "pipefail", "-c", stub + wait, env: {
        "GH_FIXTURE_COUNTER" => counter, "GH_FIXTURE_DIR" => root, "PR_NUMBER" => "159", "HEAD_SHA" => "validated-head"
      })
      assert_equal "3", File.read(counter).strip
      result
    end
    assert_includes stdout, "Required cask checks: passing; merge state: BLOCKED"
    assert_includes stdout, "Required cask checks: pending; merge state: BLOCKED"
    assert_includes stdout, "Required cask checks: passing; merge state: CLEAN"
  end

  def test_cask_validation_is_unprivileged_and_head_bound
    preparation = job(@source, "prepare-cask-bump")
    write = job(@source, "write-cask-bump")
    validation = job(@source, "validate-cask-bump")
    merge = job(@source, "merge-cask-bump")
    fetch = preparation.split("- name: Fetch cask inputs", 2).fetch(1).split("- name: Render and validate exact cask candidate", 2).first
    render = preparation.split("- name: Render and validate exact cask candidate", 2).fetch(1).split("- name: Upload cask plan", 2).first
    assert_includes fetch, "GH_TOKEN: ${{ github.token }}"
    # An unpinned curl fetch of a non-data URL would fail the workflow audit.
    refute_includes fetch, "curl"
    assert_includes fetch, "gh api"
    refute_includes fetch, "validate-cask-bump.rb"
    assert_includes preparation, "actions/download-artifact"
    ["gh release download", "releases/download", "create-github-app-token", "Homebrew/actions"].each { |text| refute_includes preparation, text }
    assert_includes render, "validate-cask-bump.rb"
    refute_includes render, "GH_TOKEN"
    assert_includes preparation, "scripts/validate-cask-bump.rb --render"
    assert_includes preparation, "scripts/validate-cask-bump.rb"
    assert_includes preparation, "cmp --silent base-cask.rb candidate-cask.rb"
    refute_match(/^\s+brew\s/, preparation)
    ["actions/checkout", "Homebrew/actions", "scripts/", "force=true"].each { |text| refute_includes write, text }
    refute_match(/^\s+brew\s/, write)
    ["-f state=all", ".tree.sha == $tree", "(.parents | length) == 1", 'if [[ "${MATCH_COUNT}" == "0" ]]'].each { |text| assert_includes write, text }
    assert_operator write.index("Validate cask plan artifact"), :<, write.index("Mint bot token for tap")
    ["actions/checkout", "tap-token.outputs.token", "APP_PRIVATE_KEY"].each { |text| refute_includes validation, text }
    ["actions/checkout", "--watch", "--fail-fast", "--auto"].each { |text| refute_includes merge, text }
    ["gh pr checks", "CHECK_STATUS=0", "8) CHECK_SUMMARY=pending", "mergeStateStatus", "CHECK_STATUS == 0",
      '[[ "${MERGE_STATE}" == "CLEAN" || "${MERGE_STATE}" == "UNSTABLE" ]]', "CHECK_TIMEOUT_SECONDS=1500",
      "no required checks appeared before the cask check timeout", "did not satisfy branch policy before the timeout",
      "a required cask check failed", '--match-head-commit "${HEAD_SHA}"'].each { |text| assert_includes merge, text }
  end
end
