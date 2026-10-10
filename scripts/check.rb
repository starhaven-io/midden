#!/usr/bin/env ruby
# frozen_string_literal: true

Encoding.default_external = Encoding::UTF_8

module Checks
  COMMANDS = [
    [%w[cargo clippy --locked --all-targets -- -D warnings]],
    [%w[cargo fmt -- --check]],
    [%w[bundle exec ruby scripts/test.rb]],
    [%w[typos], "typos", "typos-cli"],
    [%w[cargo deny check], "cargo-deny", "cargo-deny"],
    [%w[zizmor --strict-collection --persona auditor .github/workflows/], "zizmor", "zizmor"],
    [%w[lychee --config lychee.toml README.md SECURITY.md], "lychee", "lychee"],
    [%w[cargo test --locked]]
  ].freeze

  def self.executable?(name)
    ENV.fetch("PATH", "").split(File::PATH_SEPARATOR).any? do |directory|
      path = File.join(directory, name)
      File.file?(path) && File.executable?(path)
    end
  end

  def self.run
    failed = false
    skipped = []
    COMMANDS.each do |command, tool, package|
      if tool && !executable?(tool)
        puts "--- #{tool} --- skipped (#{tool} not found)"
        skipped << "#{tool} (brew install #{package})"
        failed = true
        next
      end
      puts "--- #{command.join(' ')} ---"
      $stdout.flush
      success = system(*command)
      warn "#{command.first}: command not found" if success.nil?
      failed = true unless success
    end
    unless skipped.empty?
      puts "\nChecks skipped due to missing tools:"
      skipped.each { |tool| puts "  - #{tool}" }
    end
    failed ? 1 : 0
  end
end

exit Checks.run if $PROGRAM_NAME == __FILE__
