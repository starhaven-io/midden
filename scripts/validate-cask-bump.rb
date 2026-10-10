#!/usr/bin/env ruby
# frozen_string_literal: true

Encoding.default_external = Encoding::UTF_8

require "optparse"

module CaskBump
  VERSION_LINE = /^(\s*version\s+)"([^"\n]+)"(\s*)$/
  SHA256 = /(?<![0-9a-f])[0-9a-f]{64}(?![0-9a-f])/

  def self.normalize(cask)
    version_count = 0
    normalized = cask.gsub(VERSION_LINE) do
      version_count += 1
      "#{Regexp.last_match(1)}\"__VERSION__\"#{Regexp.last_match(3)}"
    end
    raise ArgumentError, "expected one literal version line, found #{version_count}" unless version_count == 1

    hashes = normalized.scan(SHA256)
    raise ArgumentError, "expected three SHA-256 values, found #{hashes.length}" unless hashes.length == 3

    [normalized.gsub(SHA256, "__SHA256__"), hashes]
  end

  def self.validate(base, candidate, version, hashes)
    versions = candidate.scan(VERSION_LINE).map { |match| match[1] }
    raise ArgumentError, "candidate version is #{versions.inspect}, expected #{version.inspect}" unless versions == [version]

    normalized_base, = normalize(base)
    normalized_candidate, candidate_hashes = normalize(candidate)
    unless normalized_candidate == normalized_base
      raise ArgumentError, "candidate changes content other than version and SHA-256 values"
    end
    unless candidate_hashes == hashes
      raise ArgumentError, "candidate SHA-256 values do not match the published release assets"
    end
  end

  def self.render(base, version, hashes)
    if version.empty? || version.match?(/["\n\r]/)
      raise ArgumentError, "version is not safe for a literal cask version"
    end
    unless hashes.length == 3 && hashes.all? { |value| /\A[0-9a-f]{64}\z/.match?(value) }
      raise ArgumentError, "expected exactly three lowercase SHA-256 values"
    end

    normalize(base)
    candidate = base.gsub(VERSION_LINE) { "#{Regexp.last_match(1)}\"#{version}\"#{Regexp.last_match(3)}" }
    replacements = hashes.each
    candidate = candidate.gsub(SHA256) { replacements.next }
    validate(base, candidate, version, hashes)
    candidate
  end
end

if $PROGRAM_NAME == __FILE__
  render = false
  parser = OptionParser.new do |options|
    options.banner = "Usage: #{$PROGRAM_NAME} [--render] BASE CANDIDATE VERSION SHA256 SHA256 SHA256"
    options.on("--render") { render = true }
  end
  begin
    parser.parse!
    raise OptionParser::InvalidArgument, "expected six arguments" unless ARGV.length == 6
    base, candidate, version, *hashes = ARGV
    source = File.read(base, encoding: "UTF-8")
    if render
      File.write(candidate, CaskBump.render(source, version, hashes), encoding: "UTF-8")
    else
      CaskBump.validate(source, File.read(candidate, encoding: "UTF-8"), version, hashes)
    end
  rescue OptionParser::ParseError => e
    warn e.message
    warn parser
    exit 2
  rescue ArgumentError => e
    abort e.message
  end
end
