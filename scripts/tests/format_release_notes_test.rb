# frozen_string_literal: true

require_relative "test_helper"
require_relative "../format-release-notes"

class ReleaseNotesTest < Minitest::Test
  include WorkflowHelpers

  def test_bot_authors_and_new_contributor_section
    sections, = ReleaseNotes.parse_notes(<<~NOTES)
      ## What's Changed
      * fix: repair parsing by @renovate[bot] in https://example.test/1
      ## New Contributors
      * @person made their first contribution in https://example.test/2
      **Full Changelog**: https://example.test/compare/v1...v2
    NOTES
    assert_equal({ "Fixes" => ["repair parsing"] }, sections)
  end

  def test_categorizes_scoped_and_breaking_titles
    sections, changelog = ReleaseNotes.parse_notes(<<~NOTES)
      * feat!: replace the output schema by @author in https://example.test/1
      * fix(parser)!: reject ambiguous input by @author in https://example.test/2
      * docs(readme): explain migration by @author in https://example.test/3
      **Full Changelog**: https://example.test/compare/v1...v2
    NOTES
    assert_equal ["replace the output schema", "reject ambiguous input"], sections["Breaking Changes"]
    assert_equal ["explain migration"], sections["Documentation"]
    assert_equal "https://example.test/compare/v1...v2", changelog
  end

  def test_skips_internal_change_types
    sections, = ReleaseNotes.parse_notes(<<~NOTES)
      * ci: update workflow by @author in https://example.test/1
      * chore(deps): update lockfile by @author in https://example.test/2
    NOTES
    assert_empty sections
  end

  def test_breaking_internal_changes_are_preserved_and_rendered_first
    sections, = ReleaseNotes.parse_notes("* build!: raise the minimum supported compiler\n* fix: repair a thing\n")
    rendered = ReleaseNotes.format_markdown("v2.0.0", sections, nil)
    assert_includes rendered, "raise the minimum supported compiler"
    assert_operator rendered.index("Breaking Changes"), :<, rendered.index("Fixes")
  end

  def test_uncategorized_entries_are_preserved
    sections, = ReleaseNotes.parse_notes(<<~NOTES)
      * security: harden boundaries by @author in https://example.test/1
      * a title without a conventional prefix by @author in https://example.test/2
    NOTES
    assert_equal ["harden boundaries", "a title without a conventional prefix"], sections["Other"]
  end

  def test_formatting_is_stable
    rendered = ReleaseNotes.format_markdown("v1.2.3", { "Fixes" => ["repair a thing"] }, "https://example.test/compare/v1.2.2...v1.2.3")
    assert_equal <<~NOTES, rendered
      ## midden v1.2.3

      ### Fixes
      - repair a thing

      ---
      **Full Changelog**: https://example.test/compare/v1.2.2...v1.2.3
    NOTES
  end

  def test_all_splitlines_separators_preserve_sections_and_contributors
    ["\n", "\r\n", "\r", "\v", "\f", "\x1c", "\x1d", "\x1e", "\u0085", "\u2028", "\u2029"].each do |separator|
      raw = ["* fix: repair parsing", "## New Contributors", "* @person first contributed",
        "## What's Changed", "* feat: display results", "**Full Changelog**: https://example.test/compare"].join(separator)
      assert_equal [{ "Fixes" => ["repair parsing"], "What's New" => ["display results"] }, "https://example.test/compare"],
        ReleaseNotes.parse_notes(raw), separator.inspect
    end
  end

  def test_unicode_whitespace_is_trimmed_and_separates_attribution
    spaces = ["\t", "\x1f", " ", "\u00a0", "\u1680", *(0x2000..0x200a).map { |point| point.chr(Encoding::UTF_8) }, "\u202f", "\u205f", "\u3000"]
    spaces.each do |space|
      raw = "#{space}*#{space}fix:#{space}repair café#{space}by#{space}@author#{space}in#{space}https://example.test/1#{space}\n" \
        "#{space}**Full Changelog**:#{space}https://example.test/compare#{space}ignored"
      assert_equal [{ "Fixes" => ["repair café"] }, "https://example.test/compare"], ReleaseNotes.parse_notes(raw), space.inspect
    end
  end

  def test_attribution_accepts_unicode_letters_and_numbers_but_not_marks_or_connectors
    %w[Renée 日本語 Ⅳ ² user_name user-name renovate[bot]].each do |author|
      assert_equal({ "Fixes" => ["repair parsing"] },
        ReleaseNotes.parse_notes("* fix: repair parsing by @#{author} in https://example.test/1").first, author)
    end
    ["e\u0301", "user\u203fname"].each do |author|
      assert_equal({ "Fixes" => ["repair parsing by @#{author}"] },
        ReleaseNotes.parse_notes("* fix: repair parsing by @#{author} in https://example.test/1").first, author)
    end
  end

  def test_nul_and_zero_width_space_are_not_trimmed_as_whitespace
    ["\0", "\u200b"].each do |character|
      assert_empty ReleaseNotes.parse_notes("#{character}* fix: ignored").first
      assert_equal({ "Fixes" => ["preserved#{character}"] }, ReleaseNotes.parse_notes("* fix: preserved#{character}").first)
    end
  end

  def test_cli_preserves_document_newlines
    Dir.mktmpdir do |root|
      notes = File.join(root, "notes.md")
      {
        "" => "## midden v1.2.3\n\n",
        "* fix: repair a thing\n" => "## midden v1.2.3\n\n### Fixes\n- repair a thing\n\n",
        "\u00a0* fix:\u2003repair café by @Renée\u00a0\r## New Contributors\u2028* ignored\u0085**Full Changelog**:\u00a0https://example.test/compare\n" =>
          "## midden v1.2.3\n\n### Fixes\n- repair café\n\n---\n**Full Changelog**: https://example.test/compare\n\n"
      }.each do |raw, expected|
        File.write(notes, raw)
        result = capture_success(RbConfig.ruby, File.join(ROOT, "scripts/format-release-notes.rb"), notes, "v1.2.3")
        assert_equal expected, result
      end
    end
  end
end
