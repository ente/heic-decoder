require 'fileutils'
require 'open3'
require 'rbconfig'

abort 'Usage: generate-bounded.rb INCREMENTAL_CORPUS_DIRECTORY [OUTPUT_DIRECTORY]' unless (1..2).cover?(ARGV.size)
corpus = File.expand_path(ARGV[0])
source = File.join(corpus, 'direct.hevc')
abort "Missing #{source}; generate the incremental corpus first" unless File.file?(source)
root = File.expand_path('../..', __dir__)
output = File.expand_path(ARGV[1] || File.join(root, '.heic-test-assets/bounded-corpus'))
within = ->(path, directory) { path == directory || path.start_with?("#{directory}/") }
abort 'The output directory must not overlap the incremental corpus' if within.call(output, corpus) || within.call(corpus, output)
marker = File.join(output, '.generate-bounded')
if Dir.exist?(output) && !Dir.empty?(output) && !File.file?(marker)
  abort "#{output} is not empty and was not created by generate-bounded.rb"
end
FileUtils.mkdir_p(output)
File.write(marker, '')
FileUtils.rm_f(Dir.glob(File.join(output, '{pass,refuse}-*.heic')))

cases = {
  'pass-odd-width-crop' => { 'CROP' => '131x192' },
  'pass-odd-width-crop-rotate-270' => { 'CROP' => '131x192', 'ROTATION' => '3' },
  'pass-odd-height-crop-rotate-90' => { 'CROP' => '192x129', 'ROTATION' => '1' },
  'pass-odd-canvas-grid-rotate-270' => { 'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '3' },
  'pass-nclx-display-p3' => { 'TILE_NCLX' => '12/13/6/0' },
  'pass-nclx-bt2020' => { 'TILE_NCLX' => '9/1/9/0' },
  'refuse-odd-width-crop-rotate-90' => { 'CROP' => '131x192', 'ROTATION' => '1' },
  'refuse-odd-width-crop-rotate-180' => { 'CROP' => '131x192', 'ROTATION' => '2' },
  'refuse-odd-height-crop-rotate-180' => { 'CROP' => '192x129', 'ROTATION' => '2' },
  'refuse-odd-height-crop-rotate-270' => { 'CROP' => '192x129', 'ROTATION' => '3' },
  'refuse-odd-width-crop-mirror-vertical-axis' => { 'CROP' => '131x192', 'MIRROR' => '0' },
  'refuse-odd-width-crop-mirror-horizontal-axis' => { 'CROP' => '131x192', 'MIRROR' => '1' },
  'refuse-odd-canvas-grid-rotate-180' => { 'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '2' }
}
keys = %w[GRID CANVAS CROP ROTATION MIRROR TILE_NCLX PRIMARY_NCLX REFERENCE_COUNT]
cases.each do |name, options|
  path = File.join(output, "#{name}.heic")
  environment = keys.to_h { |key| [key, nil] }.merge(options)
  _, stderr, status = Open3.capture3(environment, RbConfig.ruby, File.join(__dir__, 'wrap.rb'), source, path,
                                     '256', '256', '1', '1')
  abort "wrap.rb failed for #{name}:\n#{stderr}" unless status.success?
  puts "Generated #{path}"
end
