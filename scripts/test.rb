#!/usr/bin/env ruby
# frozen_string_literal: true

Encoding.default_external = Encoding::UTF_8

tests = Dir[File.join(__dir__, "tests/*_test.rb")].sort
abort "No script tests found in #{File.join(__dir__, 'tests')}" if tests.empty?

tests.each { |path| require path }
