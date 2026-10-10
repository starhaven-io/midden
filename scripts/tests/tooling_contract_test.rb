# frozen_string_literal: true

require_relative "test_helper"
require "bundler"

class ToolingContractTest < Minitest::Test
  include WorkflowHelpers

  def test_empty_test_directory_fails_closed
    Dir.mktmpdir do |root|
      runner = File.join(root, "test.rb")
      FileUtils.cp(File.join(ROOT, "scripts/test.rb"), runner)
      _stdout, stderr, status = capture(RbConfig.ruby, runner)
      refute status.success?
      assert_includes stderr, "No script tests found"
    end
  end

  def test_frozen_bundle_accepts_a_different_ruby_patch_than_the_lockfile
    Dir.mktmpdir do |root|
      FileUtils.cp(File.join(ROOT, "Gemfile"), root)
      File.write(File.join(root, ".ruby-version"), RUBY_VERSION + "\n")
      locked_ruby = RUBY_VERSION == "4.0.6" ? "4.0.5" : "4.0.6"
      refute_equal RUBY_VERSION, locked_ruby
      lock = File.read(File.join(ROOT, "Gemfile.lock")).sub(/^  ruby [^\n]+$/, "  ruby #{locked_ruby}")
      lock_path = File.join(root, "Gemfile.lock")
      File.write(lock_path, lock)
      bundle_path = Bundler.settings[:path]
      # Bundler 4 exports BUNDLE_LOCKFILE; inheriting it silently bypasses this fixture.
      environment = ENV.keys.grep(/\A(?:BUNDLE_|BUNDLER_)/).to_h { |key| [key, nil] }.merge(
        "RUBYOPT" => nil, "RUBYLIB" => nil, "GEM_HOME" => nil, "GEM_PATH" => nil,
        "BUNDLE_LOCKFILE" => nil,
        "BUNDLE_GEMFILE" => File.join(root, "Gemfile"),
        "BUNDLE_PATH" => bundle_path && File.expand_path(bundle_path, Bundler.root),
        "BUNDLE_PATH__SYSTEM" => bundle_path ? nil : "true",
        "BUNDLE_FROZEN" => "true", "BUNDLE_APP_CONFIG" => File.join(root, ".bundle"),
        "BUNDLE_USER_CONFIG" => File.join(root, "user-bundle-config")
      )
      command = [RbConfig.ruby, Gem.bin_path("bundler", "bundle"), "exec", RbConfig.ruby, "-e",
        'puts Bundler.default_lockfile.realpath; puts Bundler.locked_gems.ruby_version']
      output = capture_success(*command, chdir: root, env: environment)
      assert_equal "#{File.realpath(lock_path)}\nruby #{locked_ruby}\n", output
      assert_equal lock, File.read(lock_path), "frozen mode must not rewrite the lock"

      gemfile = File.join(root, "Gemfile")
      File.write(gemfile, File.read(gemfile).sub(/^ruby .*$/, 'ruby file: ".ruby-version"'))
      _stdout, stderr, status = capture(*command, chdir: root, env: environment)
      refute status.success?, "the exact-patch mutant must fail against the mismatched frozen lock"
      assert_match(/unlocking ruby.*frozen mode/m, stderr)
    end
  end

  def test_unicode_release_notes_and_cask_paths_do_not_require_a_locale
    Dir.mktmpdir do |root|
      env = { "LANG" => nil, "LC_ALL" => "C" }
      notes = File.join(root, "café notes.md")
      File.write(notes, "* fix: preserve café and 日本語\n")
      output = capture_success(RbConfig.ruby, File.join(ROOT, "scripts/format-release-notes.rb"), notes, "v1.2.3", env: env)
      assert_includes output, "- preserve café and 日本語\n"
      base = File.join(root, "café base.rb")
      candidate = File.join(root, "café candidate.rb")
      content = "cask \"midden\" do\n  version \"1.0.0\"\n  desc \"café 日本語\"\n" +
        %w[1 2 3].map { |value| "  sha256 \"#{value * 64}\"\n" }.join + "end\n"
      File.write(base, content)
      hashes = %w[a b c].map { |value| value * 64 }
      capture_success(RbConfig.ruby, File.join(ROOT, "scripts/validate-cask-bump.rb"), "--render", base, candidate, "1.1.0", *hashes, env: env)
      assert_includes File.read(candidate), "desc \"café 日本語\""
      capture_success(RbConfig.ruby, File.join(ROOT, "scripts/validate-cask-bump.rb"), base, candidate, "1.1.0", *hashes, env: env)
    end
  end
end
