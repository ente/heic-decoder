require 'digest'
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'

abort 'Usage: generate.rb ORACLE LIBHEIF_CORPUS_IMAGE [OUTPUT_DIRECTORY]' unless (2..3).cover?(ARGV.size)
oracle, source = ARGV.take(2).map { |path| File.expand_path(path) }
root = File.expand_path('../..', __dir__)
output = File.expand_path(ARGV[2] || File.join(root, '.heic-test-assets/incremental-corpus'))
FileUtils.mkdir_p(output)
FileUtils.rm_f(File.join(output, 'manifest.json'))

def execute(*args)
  stdout, stderr, status = Open3.capture3(*args)
  raise "Command failed: #{args.inspect}\n#{stderr}\n#{stdout}" unless status.success?
  stdout
end

def oracle_png(oracle, input, output)
  metadata = JSON.parse(execute(oracle, input, output, 'strict'))
  raise 'Oracle recovered from errors' unless metadata.fetch('strict') && metadata.fetch('warnings').zero?
end

seed = File.join(output, 'source.png')
oracle_png(oracle, source, seed)
encodes = [['direct', 256, 256], ['tall', 256, 2048], ['pipeline', 1024, 1088], ['edge', 256, 256], ['vertical', 256, 256],
           ['edge422', 256, 256]]
encodes.each do |name, width, height|
  filters = "scale=#{width}:#{height},format=#{name.end_with?('422') ? 'yuv422p' : 'yuv420p'}"
  filters += ",geq=lum='lum(X,Y)':cb='16+mod(X*13+Y*7,224)':cr='16+mod(X*5+Y*17,224)'" if %w[edge vertical edge422].include?(name)
  filters += ',transpose=clock' if name == 'vertical'
  parameters = 'keyint=1:wpp=0:repeat-headers=1:pools=none'
  parameters += ':lossless=1' if name == 'vertical'
  execute('ffmpeg', '-v', 'error', '-y', '-i', seed, '-vf', filters, '-frames:v', '1',
          '-c:v', 'libx265', '-preset', 'fast', '-crf', '28', '-x265-params', parameters,
          '-f', 'hevc', File.join(output, "#{name}.hevc"))
end

cases = [
  ['direct', 'direct', 1, 1, {}],
  ['tall', 'tall', 1, 1, {}],
  ['pipeline', 'pipeline', 1, 1, {}],
  ['grid', 'direct', 2, 2, {}],
  ['grid-pipeline', 'pipeline', 2, 2, {}],
  ['crop', 'tall', 1, 1, { 'CROP' => '252x2040' }],
  ['oriented', 'direct', 1, 1, { 'CROP' => '130x190', 'ROTATION' => '1', 'MIRROR' => '1' }],
  ['odd-short', 'direct', 1, 1, { 'CROP' => '130x190' }],
  ['odd-tall', 'tall', 1, 1, { 'CROP' => '130x1982' }],
  ['odd-pipeline', 'pipeline', 1, 1, { 'CROP' => '898x1022' }],
  ['odd-width-grid', 'edge', 1, 1, { 'GRID' => '1', 'CANVAS' => '255x256', 'CROP' => '253x254' }],
  ['odd-height-grid', 'vertical', 1, 1, { 'GRID' => '1', 'CANVAS' => '256x255', 'CROP' => '254x253' }],
  ['undefined-grid-nclx', 'direct', 2, 2, { 'TILE_NCLX' => '1/13/1/0', 'PRIMARY_NCLX' => '2/2/2/1' }],
  ['invalid-grid-references', 'direct', 2, 2, { 'REFERENCE_COUNT' => '65535' }],
  ['tile-crop/odd-tile-crop', 'edge', 1, 1, { 'GRID' => '1', 'CANVAS' => '253x254', 'TILE_CROP' => '253x254' }],
  ['tile-crop/odd-tile-vertical', 'vertical', 1, 1, { 'GRID' => '1', 'CANVAS' => '254x253', 'TILE_CROP' => '254x253' }],
  ['tile-crop/odd-tile-photo', 'direct', 1, 1, { 'GRID' => '1', 'CANVAS' => '254x254', 'TILE_CROP' => '254x254' }],
  ['orientation/odd-width-grid-quarter-turn', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '1', 'CROP' => '254x253', 'TRANSFORM_ORDER' => 'ROTATION,CROP' }],
  ['orientation/odd-width-grid-half-turn', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '255x256', 'ROTATION' => '2', 'CROP' => '253x254', 'TRANSFORM_ORDER' => 'ROTATION,CROP' }],
  ['orientation/odd-height-grid-three-quarter-turn', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '256x255', 'ROTATION' => '3', 'CROP' => '253x254', 'TRANSFORM_ORDER' => 'ROTATION,CROP' }],
  ['orientation/odd-width-grid-mirror', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '255x256', 'MIRROR' => '1', 'CROP' => '253x254', 'TRANSFORM_ORDER' => 'MIRROR,CROP' }],
  ['orientation/odd-height-grid-mirror', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '256x255', 'MIRROR' => '1', 'CROP' => '254x253', 'TRANSFORM_ORDER' => 'MIRROR,CROP' }],
  ['orientation/odd-height-grid-turn-then-mirror', 'edge', 1, 1,
   { 'GRID' => '1', 'CANVAS' => '256x255', 'ROTATION' => '1', 'MIRROR' => '0', 'CROP' => '253x254',
     'TRANSFORM_ORDER' => 'ROTATION,MIRROR,CROP' }],
  ['orientation/odd-height-grid-quarter-turn', 'edge', 1, 1, { 'GRID' => '1', 'CANVAS' => '256x255', 'ROTATION' => '1' }],
  ['orientation/odd-three-quarter-turn', 'direct', 1, 1, { 'CROP' => '131x192', 'ROTATION' => '3' }],
  ['orientation/chroma422-quarter-turn', 'edge422', 1, 1, { 'ROTATION' => '1' }],
  ['orientation/chroma422-odd-half-turn', 'edge422', 1, 1, { 'CROP' => '131x192', 'ROTATION' => '2' }],
  ['orientation/chroma422-odd-height-half-turn', 'edge422', 1, 1, { 'CROP' => '132x191', 'ROTATION' => '2' }],
  ['orientation/chroma422-odd-mirror', 'edge422', 1, 1, { 'CROP' => '131x192', 'MIRROR' => '1' }],
  ['orientation/chroma422-odd-vertical-mirror', 'edge422', 1, 1, { 'CROP' => '131x192', 'MIRROR' => '0' }]
]
cases.each do |name, encoded, columns, rows, options|
  _, width, height = encodes.find { |entry| entry[0] == encoded }
  extension = name == 'invalid-grid-references' ? 'bin' : 'heic'
  path = File.join(output, "#{name}.#{extension}")
  FileUtils.mkdir_p(File.dirname(path))
  environment = %w[GRID CANVAS CROP TILE_CROP ROTATION MIRROR TRANSFORM_ORDER TILE_NCLX PRIMARY_NCLX REFERENCE_COUNT].to_h { |key| [key, nil] }.merge(options)
  execute(environment, RbConfig.ruby, File.join(__dir__, 'wrap.rb'), File.join(output, "#{encoded}.hevc"),
          path, width.to_s, height.to_s, columns.to_s, rows.to_s)
  oracle_png(oracle, path, path.sub(/\.heic$/, '.png')) if extension == 'heic'
  puts "Generated #{path}"
end
manifest = {
  source: { path: source, sha256: Digest::SHA256.file(source).hexdigest },
  oracle_sha256: Digest::SHA256.file(oracle).hexdigest,
  ffmpeg: execute('ffmpeg', '-version').lines.first.strip,
  inputs: cases.to_h do |name, *_|
    path = File.join(output, "#{name}.#{name == 'invalid-grid-references' ? 'bin' : 'heic'}")
    [path.delete_prefix("#{output}/"), Digest::SHA256.file(path).hexdigest]
  end
}
File.write(File.join(output, 'manifest.json'), JSON.pretty_generate(manifest))
