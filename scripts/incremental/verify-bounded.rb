require 'digest'
require 'fileutils'
require 'json'
require 'open3'
require 'timeout'

required = []
arguments = []
until ARGV.empty?
  argument = ARGV.shift
  if argument == '--require-pass'
    abort '--require-pass needs a file' if ARGV.empty?
    required << File.expand_path(ARGV.shift)
  else
    arguments << argument
  end
end
abort 'Usage: verify-bounded.rb [--require-pass FILE]... ORACLE RUNNER COMPARATOR INPUT_OR_DIRECTORY...' if arguments.size < 4
oracle, runner, comparator, *sources = arguments.map { |p| File.expand_path(p) }
root = File.expand_path('../..', __dir__)
output = File.expand_path(ENV.fetch('BOUNDED_TEST_ROOT', File.join(root, '.heic-test-runs/bounded')))
budget_mib = Integer(ENV.fetch('BOUNDED_BUDGET_MIB', '512'))
sides = [6000, 97]
refusals = %w[unsupported limit_exceeded memory_budget_exceeded]
fatal = %w[timeout crash allocation_failed unexpected_error bounded_only dimension_mismatch pixel_mismatch
           oracle_timeout oracle_crash]
FileUtils.mkdir_p(output)

inputs = sources.flat_map do |source|
  if File.directory?(source)
    Dir.glob(File.join(source, '**', '*')).select { |p| File.file?(p) && p.match?(/\.(heic|heif|avif)\z/i) }
  else
    [source]
  end
end.uniq.sort
abort 'No inputs found' if inputs.empty?
missing = required - inputs
abort "Required inputs not found: #{missing.join(', ')}" unless missing.empty?

def run(*args)
  Open3.popen3(*args, pgroup: true) do |input, out, err, thread|
    input.close
    stdout = Thread.new { out.read }
    stderr = Thread.new { err.read }
    begin
      status = Timeout.timeout(120) { thread.value }
      [status, stdout.value, stderr.value]
    rescue Timeout::Error
      Process.kill('KILL', -thread.pid)
      thread.value
      [nil, stdout.value, stderr.value]
    end
  end
end

def oriented(width, height, orientation)
  orientation >= 5 ? [height, width] : [width, height]
end

manifest = {
  binaries: [oracle, runner, comparator].to_h { |p| [p, Digest::SHA256.file(p).hexdigest] },
  inputs: inputs.size,
  required: required,
  budget_mib: budget_mib,
  sides: sides,
  profile: 'rgb8-rounding'
}
File.write(File.join(output, 'manifest.json'), JSON.pretty_generate(manifest))
counts = Hash.new(0)
failures = []
File.open(File.join(output, 'results.jsonl'), 'w') do |report|
  inputs.each do |path|
    dir = File.join(output, Digest::SHA256.hexdigest(path))
    FileUtils.mkdir_p(dir)
    png = File.join(dir, 'reference.png')
    status, stdout, stderr = run(oracle, path, png, 'strict')
    oracle_outcome =
      if status.nil?
        'oracle_timeout'
      elsif status.exitstatus.nil? || ![0, 1, 2].include?(status.exitstatus)
        'oracle_crash'
      end
    oracle_error = stderr.strip unless status&.success?
    reference = status&.success? ? JSON.parse(stdout) : nil
    if oracle_outcome
      counts[oracle_outcome] += 1
      failures << "#{path}: #{oracle_outcome}: #{oracle_error}"
    end
    sides.each do |side|
      raw = File.join(dir, 'actual.rgb')
      FileUtils.rm_f(raw)
      status, stdout, stderr = run(runner, 'decode-bounded', path, side.to_s, budget_mib.to_s, raw)
      decoded = begin
        JSON.parse(stdout.lines.last.to_s)
      rescue JSON::ParserError
        nil
      end
      entry = { path: path, side: side, reference: reference, oracle_error: oracle_error, decoded: decoded }
      outcome =
        if status.nil?
          'timeout'
        elsif decoded.nil? || ![0, 1].include?(status.exitstatus)
          entry[:stderr] = stderr.lines.last(20).join
          'crash'
        elsif status.exitstatus == 1
          if decoded['kind'] == 'allocation_failed'
            'allocation_failed'
          elsif reference.nil?
            'both_refused'
          elsif refusals.include?(decoded['kind'])
            'refused'
          else
            'unexpected_error'
          end
        elsif reference.nil?
          'bounded_only'
        else
          orientation = decoded.fetch('exif_orientation')
          expected = oriented(reference.fetch('width'), reference.fetch('height'), orientation)
          if [decoded.fetch('original_width'), decoded.fetch('original_height')] != expected
            entry[:expected_original] = expected
            'dimension_mismatch'
          else
            primaries = reference.dig('nclx', 'primaries') || 0
            status, stdout, stderr = run(comparator, 'png', png, raw, decoded.fetch('width').to_s,
                                         decoded.fetch('height').to_s, side.to_s, 'rgb8-rounding',
                                         orientation.to_s, primaries.to_s)
            entry[:metrics] = JSON.parse(stdout) rescue nil
            entry[:comparator] = stderr.strip
            status&.success? ? 'pass' : 'pixel_mismatch'
          end
        end
      entry[:outcome] = outcome
      report.puts(JSON.generate(entry))
      report.flush
      FileUtils.rm_f(raw)
      counts[outcome] += 1
      if fatal.include?(outcome)
        detail = decoded&.fetch('error', nil) || entry[:comparator] || entry[:stderr] || oracle_error
        failures << "#{path} (side #{side}): #{outcome}: #{detail}"
      elsif required.include?(path) && outcome != 'pass'
        failures << "#{path} (side #{side}): required to pass but #{outcome}: #{decoded&.fetch('error', nil)}"
      end
    end
    FileUtils.rm_f(png)
  end
end
summary = counts.sort.map { |outcome, count| "#{outcome}=#{count}" }.join(' ')
File.write(File.join(output, 'summary.txt'), "#{summary}\n#{failures.join("\n")}\n")
puts summary
failures.each { |failure| puts "FAIL #{failure}" }
exit(failures.empty? && counts['pass'].positive? ? 0 : 1)
