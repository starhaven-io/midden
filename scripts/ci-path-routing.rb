#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"

module CiPathRouting
  FULL_MATRIX = [
    { "name" => "Lint", "check" => "lint", "runner" => "ubuntu-24.04" },
    { "name" => "Test (Linux)", "check" => "test", "runner" => "ubuntu-24.04" },
    { "name" => "Test (Linux ARM)", "check" => "test", "runner" => "ubuntu-24.04-arm" },
    { "name" => "Test (macOS)", "check" => "test", "runner" => "macos-26" },
    { "name" => "Coverage", "check" => "coverage", "runner" => "ubuntu-24.04" },
    { "name" => "MSRV", "check" => "msrv", "runner" => "ubuntu-24.04" }
  ].freeze
  LINT_MATRIX = [FULL_MATRIX.first].freeze
  RUST_OR_BUILD = %r{(?:\.rs$|^Cargo\.(?:toml|lock)$|^\.cargo/config(?:\.toml)?$|^rust-toolchain(?:\.toml)?$|^\.?clippy\.toml$|^\.?rustfmt\.toml$|^\.config/nextest\.toml$|^tests/|^\.github/workflows/(?:ci|cargo-deny|release)\.yml$)}
  LINT_ONLY = %r{(?:^deny\.toml$|^_typos\.toml$|^justfile$|^Gemfile(?:\.lock)?$|^\.ruby-version$|^scripts/.*\.rb$|^scripts/tests/)}
  LINKS = %r{(?:^(?:README|SECURITY)\.md$|^lychee\.toml$|^\.github/workflows/(?:ci|link-check)\.yml$)}

  def self.read_paths(input)
    bytes = input.b
    unless bytes.empty? || bytes.end_with?("\0")
      raise ArgumentError, "expected NUL-terminated changed paths"
    end
    bytes.split("\0")
  end

  def self.route(paths)
    matrix = if paths.any? { |path| RUST_OR_BUILD.match?(path) }
      FULL_MATRIX
    elsif paths.any? { |path| LINT_ONLY.match?(path) }
      LINT_MATRIX
    else
      []
    end
    {
      "matrix" => matrix,
      "links" => paths.any? { |path| LINKS.match?(path) },
      "zizmor" => paths.any? { |path| path.start_with?(".github/workflows/") },
      "coverage" => matrix == FULL_MATRIX
    }
  end
end

if $PROGRAM_NAME == __FILE__
  CiPathRouting.route(CiPathRouting.read_paths($stdin.binmode.read)).each do |key, value|
    puts "#{key}=#{JSON.generate(value)}"
  end
end
