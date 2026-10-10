# frozen_string_literal: true

Encoding.default_external = Encoding::UTF_8

require "minitest/autorun"
require "fileutils"
require "json"
require "open3"
require "rbconfig"
require "shellwords"
require "tmpdir"

module WorkflowHelpers
  ROOT = File.expand_path("../..", __dir__)

  def workflow(name)
    File.read(File.join(ROOT, ".github/workflows", name), encoding: "UTF-8")
  end

  def dedent(text)
    indentation = text.lines.reject { |line| line.strip.empty? }.map { |line| line[/\A */].length }.min || 0
    text.gsub(/^ {0,#{indentation}}/, "")
  end

  def job(source, name)
    source.match(/^  #{Regexp.escape(name)}:\n.*?(?=^  [a-zA-Z0-9_-]+:\n|\z)/m)&.to_s || raise("job not found: #{name}")
  end

  def run_blocks(source)
    source.scan(/^([ ]*)run:\s*\|\s*\n((?:\n|\1 +[^\n]*\n?)*)/).map { |_indent, block| dedent(block) }
  end

  def workflow_run_block(source, name)
    step = source.split("      - name: #{name}\n", 2).fetch(1).split("\n      - ", 2).first
    run_blocks(step).first || raise("missing run block: #{name}")
  end

  def capture(*command, env: {}, **options)
    Open3.capture3(env, *command, **options)
  end

  def capture_success(*command, **options)
    stdout, stderr, status = capture(*command, **options)
    assert status.success?, stderr
    stdout
  end

  def write_executable(path, content)
    File.write(path, content)
    File.chmod(0o755, path)
  end
end
