Pod::Spec.new do |s|
  s.name = 'StatementPdf'
  s.version = '0.1.0'
  s.summary = 'Local PDF statement extraction'
  s.description = 'Extracts selectable PDF text and computes statement fingerprints on iOS.'
  s.license = 'GPL-3.0-only'
  s.author = 'Peanut Count'
  s.homepage = 'https://github.com/dingyuchen/peanut-count'
  s.source = { :path => '.' }
  s.platforms = { :ios => '16.4' }
  s.swift_version = '5.9'
  s.static_framework = true
  s.dependency 'ExpoModulesCore'
  s.source_files = '**/*.swift'
end
