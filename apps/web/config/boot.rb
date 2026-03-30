ENV["BUNDLE_GEMFILE"] ||= File.expand_path("../Gemfile", __dir__)

def load_env_file(path)
  return unless File.exist?(path)

  File.foreach(path) do |line|
    stripped = line.strip
    next if stripped.empty? || stripped.start_with?("#")

    key, raw_value = line.split("=", 2)
    next unless key && raw_value

    key = key.strip
    next if key.empty? || ENV.key?(key)

    value = raw_value.strip
    if (value.start_with?('"') && value.end_with?('"')) ||
       (value.start_with?("'") && value.end_with?("'"))
      value = value[1...-1]
    end

    ENV[key] = value
  end
end

web_root = File.expand_path("..", __dir__)
repo_root = File.expand_path("../..", web_root)

[
  File.join(web_root, ".env"),
  File.join(web_root, ".env.local"),
  File.join(repo_root, ".env"),
  File.join(repo_root, ".env.local")
].each do |path|
  load_env_file(path)
end

require "bundler/setup" # Set up gems listed in the Gemfile.
require "bootsnap/setup" # Speed up boot time by caching expensive operations.
