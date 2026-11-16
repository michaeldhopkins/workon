# frozen_string_literal: true

# Loaded with RUBYOPT=-r from workon's data directory, before the app boots.
# Suffixes development and test database names by WORKON_RAILS_DB_SUFFIX.
# The suffix is the only per-workspace input, and it comes from the environment.
# Replicas, databases with database_tasks false, other environments, and SQLite
# are left as config/database.yml wrote them.

require "uri"

module WorkonRailsDbSuffix
  IDENTIFIER_LIMIT = 63

  class << self
    def install
      return if @installed
      return unless defined?(ActiveRecord::DatabaseConfigurations)
      handlers = ActiveRecord::DatabaseConfigurations
      return unless handlers.respond_to?(:register_db_config_handler)

      handlers.register_db_config_handler do |env_name, name, url, config|
        build(env_name, name, url, config)
      end
      @installed = true
    end

    def build(env_name, name, url, config)
      suffix = usable_suffix
      return nil if suffix.nil?
      return nil unless env_name == "development" || env_name == "test"
      return nil if replica?(config) || tasks_disabled?(config)

      if url.is_a?(String) && !url.empty?
        return nil if sqlite_url?(url)

        suffixed = suffix_url(url, suffix)
        return nil if suffixed.nil? || suffixed == url

        ActiveRecord::DatabaseConfigurations::UrlConfig.new(env_name, name, suffixed, config)
      else
        return nil if sqlite_adapter?(config)

        database = value(config, :database)
        return nil unless database.is_a?(String) && !database.empty?

        suffixed = suffix_name(database, suffix)
        return nil if suffixed == database

        ActiveRecord::DatabaseConfigurations::HashConfig.new(env_name, name, config.merge(database: suffixed))
      end
    end

    def usable_suffix
      suffix = ENV.fetch("WORKON_RAILS_DB_SUFFIX", "")
      suffix.match?(/\A_[A-Za-z0-9_]*\z/) ? suffix : nil
    end

    # HashConfig#replica? is the raw value, and only false and nil are not a replica.
    def replica?(config)
      !!value(config, :replica)
    end

    # `database_tasks: false` is a database Rails was told not to create.
    def tasks_disabled?(config)
      flag = value(config, :database_tasks)
      flag == false || flag == "false"
    end

    def sqlite_adapter?(config)
      %w[sqlite sqlite3].include?(value(config, :adapter).to_s)
    end

    def sqlite_url?(url)
      scheme = url.split(":", 2).first
      %w[sqlite sqlite3].include?(scheme)
    end

    # `false` is a real value. `||` would skip it and read the other key.
    def value(config, key)
      return config[key] if config.key?(key)

      config[key.to_s] if config.key?(key.to_s)
    end

    # Keep the suffix, and shorten the base so the identifier fits in Postgres.
    # The tail of the base stays: `app_development` and `app_test` share a prefix,
    # and the tail is what tells them apart.
    def suffix_name(name, suffix)
      return name if name.end_with?(suffix)

      suffix_bytes = suffix.b
      if suffix_bytes.bytesize >= IDENTIFIER_LIMIT
        return suffix_bytes.byteslice(0, IDENTIFIER_LIMIT).scrub("")
      end

      budget = IDENTIFIER_LIMIT - suffix_bytes.bytesize
      base = name.b
      if base.bytesize > budget
        base = base.byteslice(base.bytesize - budget, budget).scrub("")
      end
      "#{base}#{suffix}"
    end

    def suffix_url(url, suffix)
      uri = URI.parse(url)
      return nil if uri.scheme.nil? || %w[sqlite sqlite3].include?(uri.scheme)

      database = uri.path.to_s.delete_prefix("/")
      return nil if database.empty?

      suffixed = suffix_name(database, suffix)
      return url if suffixed == database

      uri.path = "/#{suffixed}"
      uri.to_s
    rescue URI::InvalidURIError
      nil
    end
  end
end

# The class body registers Rails' own handler before it ends. Registering on
# that :end event puts ours after it, and handlers run last-registered first.
unless WorkonRailsDbSuffix.instance_variable_get(:@armed)
  WorkonRailsDbSuffix.instance_variable_set(:@armed, true)
  if defined?(ActiveRecord::DatabaseConfigurations)
    WorkonRailsDbSuffix.install
  else
    trace = TracePoint.new(:end) do |event|
      next unless event.self.respond_to?(:name)
      next unless event.self.name == "ActiveRecord::DatabaseConfigurations"

      WorkonRailsDbSuffix.install
      trace.disable
    end
    trace.enable
  end
end
