# frozen_string_literal: true

# Loaded with RUBYOPT=-r from workon's data directory, before the app boots.
# Suffixes development and test database names by WORKON_RAILS_DB_SUFFIX.
# The suffix is the only per-workspace input, and it comes from the environment.
# Remote replicas, databases with database_tasks false, other environments, and
# SQLite are left as config/database.yml wrote them. A replica with no host, a
# localhost host, or a socket is suffixed so a read cannot use the primary checkout.

module WorkonRailsDbSuffix
  # `require` only. Wrapping `require_relative` resolves the path against
  # this file, so Active Record cannot load the sibling it asked for.
  module RequireHook
    def require(path)
      return super if WorkonRailsDbSuffix.installed?

      loaded = super
      WorkonRailsDbSuffix.install_if_ready
      loaded
    end
  end

  # HashConfig#initialize runs before UrlConfig merges the URL and coerces
  # "false". Skip that pass for a UrlConfig and apply once, afterwards.
  module HashPatch
    def initialize(...)
      super
      WorkonRailsDbSuffix.apply(self) unless is_a?(ActiveRecord::DatabaseConfigurations::UrlConfig)
    end
  end

  module UrlPatch
    def initialize(...)
      super
      WorkonRailsDbSuffix.apply(self)
    end
  end

  class << self
    def installed?
      @installed == true
    end

    def install_if_ready
      return if installed? || @installing

      @installing = true
      ready = defined?(ActiveRecord::DatabaseConfigurations::HashConfig) &&
        defined?(ActiveRecord::DatabaseConfigurations::UrlConfig)
      if ready
        ActiveRecord::DatabaseConfigurations::HashConfig.prepend(HashPatch)
        ActiveRecord::DatabaseConfigurations::UrlConfig.prepend(UrlPatch)
        @installed = true
      end
    ensure
      @installing = false
    end

    def apply(config)
      suffix = usable_suffix
      return if suffix.nil?
      return unless config.env_name == "development" || config.env_name == "test"
      return if sqlite?(config)
      return if remote_replica?(config) || tasks_off?(config)

      limit = identifier_limit(config)
      database = config.database
      if database.is_a?(String) && !database.empty?
        suffixed = suffix_name(database, suffix, limit)
        config._database = suffixed if suffixed != database
      end
      url = config.instance_variable_get(:@url)
      return unless url.is_a?(String) && !url.empty?

      suffixed_url = suffix_url(url, suffix, limit)
      config.instance_variable_set(:@url, suffixed_url) if suffixed_url && suffixed_url != url
    end

    def usable_suffix
      suffix = ENV.fetch("WORKON_RAILS_DB_SUFFIX", "")
      suffix.match?(/\A_[A-Za-z0-9_]+\z/) ? suffix : nil
    end

    # Rails' own predicate, after UrlConfig has turned the string "false" into false.
    def remote_replica?(config)
      return false unless config.replica?

      !local?(config)
    end

    def tasks_off?(config)
      !config.replica? && !config.database_tasks?
    end

    def local?(config)
      hash = config.configuration_hash
      socket = hash[:socket]
      return true if socket.is_a?(String) && !socket.empty?

      host = hash[:host].to_s
      host.empty? || host == "localhost" || host == "127.0.0.1" || host == "::1"
    end

    def sqlite?(config)
      adapter = config.configuration_hash[:adapter].to_s
      return true if adapter == "sqlite" || adapter == "sqlite3"

      url = config.instance_variable_get(:@url)
      return false unless url.is_a?(String) && !url.empty?

      scheme = url.split(":", 2).first
      scheme == "sqlite" || scheme == "sqlite3"
    end

    def identifier_limit(config)
      adapter = config.configuration_hash[:adapter].to_s
      return 64 if %w[mysql mysql2 trilogy].include?(adapter)

      url = config.instance_variable_get(:@url).to_s
      return 64 if url.start_with?("mysql://", "mysql2://", "trilogy://")

      63
    end

    # Keep the suffix when it fits beside an 8-hex tag of the full name, and
    # shorten on a character boundary. Distinct names stay distinct.
    def suffix_name(name, suffix, limit)
      return name unless name.valid_encoding?
      return name if name.end_with?(suffix) && name.bytesize <= limit

      combined = +"#{name}#{suffix}"
      return combined if combined.bytesize <= limit
      return utf8_prefix("#{tag_of(name)}#{suffix}", limit) if limit <= 8

      if suffix.bytesize + 8 <= limit
        tail = utf8_tail(name, limit - suffix.bytesize - 8)
        "#{tail}#{tag_of(name)}#{suffix}"
      else
        "#{tag_of(name)}#{utf8_prefix(suffix, limit - 8)}"
      end
    end

    def tag_of(name)
      require "digest"
      Digest::SHA256.hexdigest(name)[0, 8]
    end

    def utf8_tail(text, budget)
      return +"" if budget <= 0

      out = +""
      text.chars.reverse_each do |char|
        break if out.bytesize + char.bytesize > budget

        out.prepend(char)
      end
      out
    end

    def utf8_prefix(text, budget)
      return +"" if budget <= 0

      out = +""
      text.each_char do |char|
        break if out.bytesize + char.bytesize > budget

        out << char
      end
      out
    end

    def suffix_url(url, suffix, limit)
      require "uri"
      uri = URI.parse(url)
      return nil if uri.scheme.nil? || %w[sqlite sqlite3].include?(uri.scheme)

      database = uri.path.to_s.delete_prefix("/")
      return nil if database.empty?

      suffixed = suffix_name(database, suffix, limit)
      return url if suffixed == database

      uri.path = "/#{suffixed}"
      uri.to_s
    rescue URI::InvalidURIError
      nil
    end
  end
end

unless WorkonRailsDbSuffix.instance_variable_get(:@armed)
  WorkonRailsDbSuffix.instance_variable_set(:@armed, true)
  Kernel.prepend(WorkonRailsDbSuffix::RequireHook)
  WorkonRailsDbSuffix.install_if_ready
end
