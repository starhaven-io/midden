#!/usr/bin/env ruby
# frozen_string_literal: true

Encoding.default_external = Encoding::UTF_8

module ReleaseNotes
  SECTIONS = {
    "feat" => "What's New",
    "fix" => "Fixes",
    "perf" => "Performance",
    "refactor" => "Under the Hood",
    "docs" => "Documentation"
  }.freeze
  SKIP_TYPES = %w[build ci chore].freeze
  # Preserve Python's Unicode whitespace, word characters, and splitlines rules.
  SPACE = /[[:space:]\x1c-\x1f]/
  NONSPACE = /[^[:space:]\x1c-\x1f]/
  LINE_BREAK = /\r\n|[\n\r\v\f\x1c-\x1e\u0085\u2028\u2029]/
  PR = %r{
    \A\*#{SPACE}+
    (?:(?<type>[a-z]+)(?:\((?<scope>[^)]*)\))?(?<breaking>!)?:#{SPACE}*)?
    (?<desc>.+?)
    (?:#{SPACE}+by#{SPACE}+@[\p{L}\p{N}_-]+(?:\[bot\])?)?
    (?:#{SPACE}+in#{SPACE}+https?://#{NONSPACE}+)?#{SPACE}*\z
  }x
  CHANGELOG = %r{\A\*\*Full Changelog\*\*:#{SPACE}*(?<url>https?://#{NONSPACE}+)}

  def self.strip_whitespace(text)
    text.gsub(/\A#{SPACE}+|#{SPACE}+\z/, "")
  end

  def self.parse_notes(raw)
    sections = {}
    changelog_url = nil
    contributors = false
    raw.split(LINE_BREAK).each do |line|
      line = strip_whitespace(line)
      contributors = line == "## New Contributors" if line.start_with?("## ")
      if (match = CHANGELOG.match(line))
        changelog_url = match[:url]
        next
      end
      next if contributors

      match = PR.match(line)
      next unless match

      type = match[:type] || ""
      section = if match[:breaking]
        "Breaking Changes"
      elsif SKIP_TYPES.include?(type)
        next
      else
        SECTIONS.fetch(type, "Other")
      end
      (sections[section] ||= []) << strip_whitespace(match[:desc])
    end
    [sections, changelog_url]
  end

  def self.format_markdown(tag, sections, changelog_url)
    lines = ["## midden #{tag}", ""]
    ["Breaking Changes", *SECTIONS.values.uniq, "Other"].each do |heading|
      entries = sections[heading]
      next if entries.nil? || entries.empty?

      lines << "### #{heading}"
      entries.each { |entry| lines << "- #{entry}" }
      lines << ""
    end
    lines.concat(["---", "**Full Changelog**: #{changelog_url}", ""]) if changelog_url
    lines.join("\n")
  end
end

if $PROGRAM_NAME == __FILE__
  abort "Usage: #{$PROGRAM_NAME} <raw_notes_file> <tag>" if ARGV.length < 2
  sections, changelog_url = ReleaseNotes.parse_notes(File.read(ARGV[0], encoding: "UTF-8"))
  # Retain the CLI's terminating newline even when the rendered document has one.
  print ReleaseNotes.format_markdown(ARGV[1], sections, changelog_url), "\n"
end
