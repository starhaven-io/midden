# frozen_string_literal: true

require_relative "test_helper"
require_relative "../validate-cask-bump"

class ValidateCaskBumpTest < Minitest::Test
  include WorkflowHelpers

  OLD_HASHES = %w[1 2 3].map { |value| value * 64 }.freeze
  NEW_HASHES = %w[a b c].map { |value| value * 64 }.freeze
  BASE = <<~CASK
    cask "midden" do
      version "1.0.0"
      sha256 "#{OLD_HASHES[0]}"
      sha256 arm64_linux: "#{OLD_HASHES[1]}",
             x86_64_linux: "#{OLD_HASHES[2]}"
      binary "midden"
    end
  CASK
  CANDIDATE = OLD_HASHES.zip(NEW_HASHES).reduce(BASE.sub('version "1.0.0"', 'version "1.1.0"')) { |cask, (old, replacement)| cask.gsub(old, replacement) }

  def test_renders_an_exact_version_and_checksum_bump
    assert_equal CANDIDATE, CaskBump.render(BASE, "1.1.0", NEW_HASHES)
  end

  def test_same_version_still_reconciles_wrong_checksums
    candidate = CaskBump.render(BASE, "1.0.0", NEW_HASHES)
    refute_equal BASE, candidate
    CaskBump.validate(BASE, candidate, "1.0.0", NEW_HASHES)
  end

  def test_accepts_exact_version_and_checksum_bump
    CaskBump.validate(BASE, CANDIDATE, "1.1.0", NEW_HASHES)
  end

  def test_rejects_unrelated_candidate_code
    malicious = CANDIDATE.sub('  binary "midden"', "  preflight { system \"curl\", \"example.test\" }\n  binary \"midden\"")
    error = assert_raises(ArgumentError) { CaskBump.validate(BASE, malicious, "1.1.0", NEW_HASHES) }
    assert_match(/content other than/, error.message)
  end

  def test_rejects_unverified_checksum
    error = assert_raises(ArgumentError) { CaskBump.validate(BASE, CANDIDATE, "1.1.0", ["d" * 64, *NEW_HASHES.drop(1)]) }
    assert_match(/do not match/, error.message)
  end

  def test_rejects_nonliteral_version_logic
    candidate = CANDIDATE.sub('version "1.1.0"', 'version ENV.fetch("VERSION")')
    error = assert_raises(ArgumentError) { CaskBump.validate(BASE, candidate, "1.1.0", NEW_HASHES) }
    assert_match(/candidate version/, error.message)
  end

  def test_rejects_unsafe_versions_and_malformed_checksums
    ["", "1\n2", "1\r2", '1"2'].each do |version|
      assert_raises(ArgumentError) { CaskBump.render(BASE, version, NEW_HASHES) }
    end
    [NEW_HASHES.take(2), ["a" * 63, *NEW_HASHES.drop(1)], ["A" * 64, *NEW_HASHES.drop(1)], ["a" * 64 + "\n", *NEW_HASHES.drop(1)]].each do |hashes|
      assert_raises(ArgumentError) { CaskBump.render(BASE, "1.1.0", hashes) }
    end
  end

  def test_cli_renders_and_validates_without_evaluating_cask_code
    Dir.mktmpdir do |root|
      base = File.join(root, "base.rb")
      candidate = File.join(root, "candidate.rb")
      File.write(base, BASE)
      command = [RbConfig.ruby, File.join(ROOT, "scripts/validate-cask-bump.rb")]
      capture_success(*command, "--render", base, candidate, "1.1.0", *NEW_HASHES)
      assert_equal CANDIDATE, File.read(candidate)
      capture_success(*command, base, candidate, "1.1.0", *NEW_HASHES)
    end
  end

  def test_cli_rejects_invalid_arguments_with_usage_status
    [[], ["--unknown"]].each do |arguments|
      _stdout, stderr, status = capture(RbConfig.ruby, File.join(ROOT, "scripts/validate-cask-bump.rb"), *arguments)
      assert_equal 2, status.exitstatus
      assert_includes stderr, "Usage:"
    end
  end
end
