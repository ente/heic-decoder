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
  'pass-odd-origin-crop' => { 'CROP' => '130x190' },
  'pass-odd-origin-crop-rotate-90-mirror' => { 'CROP' => '130x190', 'ROTATION' => '1', 'MIRROR' => '1' },
  'pass-odd-canvas-grid-rotate-90-trimmed' => {
    'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '1', 'CROP' => '254x253', 'TRANSFORM_ORDER' => 'ROTATION,CROP'
  },
  'pass-odd-canvas-grid-rotate-180-trimmed' => {
    'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '2', 'CROP' => '253x254', 'TRANSFORM_ORDER' => 'ROTATION,CROP'
  },
  'pass-odd-canvas-grid-mirror-trimmed' => {
    'GRID' => '1', 'CANVAS' => '256x255', 'MIRROR' => '0', 'CROP' => '254x253', 'TRANSFORM_ORDER' => 'MIRROR,CROP'
  },
  'pass-tiled-odd-origin-crop' => { 'TILES' => '2x2', 'CROP' => '510x510' },
  'pass-tiled-rotate-90-odd-origin-crop' => {
    'TILES' => '2x2', 'ROTATION' => '1', 'CROP' => '510x508', 'TRANSFORM_ORDER' => 'ROTATION,CROP'
  },
  'pass-tiled-odd-canvas-rotate-90-trimmed' => {
    'TILES' => '2x2', 'CANVAS' => '511x512', 'ROTATION' => '1', 'CROP' => '510x509', 'TRANSFORM_ORDER' => 'ROTATION,CROP'
  },
  'pass-tiled-odd-canvas-mirror-trimmed' => {
    'TILES' => '3x2', 'CANVAS' => '767x511', 'MIRROR' => '1', 'CROP' => '765x509', 'TRANSFORM_ORDER' => 'MIRROR,CROP'
  }
}
keys = %w[GRID CANVAS CROP ROTATION MIRROR TRANSFORM_ORDER TILE_NCLX PRIMARY_NCLX REFERENCE_COUNT]
cases.each do |name, options|
  path = File.join(output, "#{name}.heic")
  columns, rows = options.fetch('TILES', '1x1').split('x')
  environment = keys.to_h { |key| [key, nil] }.merge(options.except('TILES'))
  _, stderr, status = Open3.capture3(environment, RbConfig.ruby, File.join(__dir__, 'wrap.rb'), source, path,
                                     '256', '256', columns, rows)
  abort "wrap.rb failed for #{name}:\n#{stderr}" unless status.success?
  puts "Generated #{path}"
end
