# frozen_string_literal: true

require_relative "test_helper"
require "yaml"

class CIConclusionTest < Minitest::Test
  include WorkflowHelpers

  def setup
    @conclusion = YAML.safe_load(workflow("ci.yml")).fetch("jobs").fetch("conclusion")
    @result_step = @conclusion.fetch("steps").find { |step| step["name"] == "Result" }
    @script = @result_step.fetch("run")
    @environment = {
      "GITHUB_EVENT_NAME" => "pull_request", "GENERATE_RESULT" => "success",
      "COMMITS_RESULT" => "success", "FLEET_RESULT" => "success", "CHECK_RESULT" => "success",
      "CODECOV_RESULT" => "success", "LINKS_RESULT" => "success", "ZIZMOR_RESULT" => "success",
      "PINPRICK_RESULT" => "success", "EVENT_NAME" => "pull_request", "MATRIX" => '[{"check":"coverage"}]',
      "COVERAGE" => "true", "CODECOV_ELIGIBLE" => "true", "LINKS" => "true", "ZIZMOR" => "true"
    }
  end

  def conclude(overrides = {})
    capture("/bin/bash", "-euo", "pipefail", "-c", @script, env: @environment.merge(overrides)).last
  end

  def test_result_receives_every_direct_dependency_and_routing_decision
    results = {
      "generate-matrix" => "GENERATE_RESULT", "commits" => "COMMITS_RESULT", "fleet" => "FLEET_RESULT",
      "check" => "CHECK_RESULT", "codecov" => "CODECOV_RESULT", "links" => "LINKS_RESULT",
      "zizmor" => "ZIZMOR_RESULT", "pinprick" => "PINPRICK_RESULT"
    }
    assert_equal results.keys.sort, @conclusion.fetch("needs").sort
    assert_equal "always()", @conclusion.fetch("if")
    environment = @result_step.fetch("env")
    results.each do |dependency, variable|
      assert_equal "${{ needs.#{dependency}.result }}", environment.fetch(variable)
    end
    %w[matrix coverage links zizmor].each do |output|
      assert_equal "${{ needs.generate-matrix.outputs.#{output} }}", environment.fetch(output.upcase)
    end
    assert_equal "${{ github.event_name }}", environment.fetch("EVENT_NAME")
    assert_equal "${{ (github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository) }}",
      environment.fetch("CODECOV_ELIGIBLE")
  end

  def test_required_audit_results_fail_closed
    assert conclude.success?
    @environment.keys.grep(/_RESULT\z/).each do |variable|
      ["failure", "cancelled", "skipped", ""].each do |result|
        refute conclude(variable => result).success?, "#{variable}=#{result.inspect}"
      end
    end
  end

  def test_only_explicitly_unselected_audits_may_skip
    assert conclude("ZIZMOR" => "false", "ZIZMOR_RESULT" => "skipped", "PINPRICK_RESULT" => "skipped").success?
    refute conclude("ZIZMOR" => "false", "ZIZMOR_RESULT" => "skipped", "PINPRICK_RESULT" => "failure").success?
    refute conclude("ZIZMOR" => "", "ZIZMOR_RESULT" => "skipped", "PINPRICK_RESULT" => "skipped").success?
  end

  def test_codecov_is_required_only_for_selected_eligible_runs
    %w[true false].product(%w[true false]).each do |coverage, eligible|
      required = coverage == "true" && eligible == "true"
      expected = required ? "success" : "skipped"
      overrides = { "COVERAGE" => coverage, "CODECOV_ELIGIBLE" => eligible }
      assert conclude(overrides.merge("CODECOV_RESULT" => expected)).success?, overrides.inspect
      (%w[success skipped failure cancelled] - [expected]).each do |result|
        refute conclude(overrides.merge("CODECOV_RESULT" => result)).success?, overrides.merge("CODECOV_RESULT" => result).inspect
      end
    end
  end

  def test_every_routing_decision_fails_closed
    [
      { "EVENT_NAME" => "unknown", "COMMITS_RESULT" => "skipped", "FLEET_RESULT" => "skipped" },
      { "MATRIX" => "", "CHECK_RESULT" => "skipped" },
      { "COVERAGE" => "", "CODECOV_RESULT" => "skipped" },
      { "CODECOV_ELIGIBLE" => "", "CODECOV_RESULT" => "skipped" },
      { "LINKS" => "", "LINKS_RESULT" => "skipped" }
    ].each { |overrides| refute conclude(overrides).success?, overrides.inspect }
  end
end
