# frozen_string_literal: true

# Proves rails_db_suffix.rb against ActiveRecord's own configuration objects.
# Loaded with RUBYOPT=-r that script. Builds configs and never connects.

begin
  require "active_record"
  require "active_record/database_configurations"
rescue LoadError => error
  puts "NO_AR #{error.message}"
  exit 0
end

ENV["RAILS_ENV"] = "development"
SUFFIX = ENV.fetch("WORKON_RAILS_DB_SUFFIX")

def configs_for(hash)
  ActiveRecord::DatabaseConfigurations.new(hash).configurations
end

def database_of(hash, env, name = "primary")
  found = configs_for(hash).find { |config| config.env_name == env && config.name == name }
  raise "no #{env} #{name} in #{hash.inspect}" if found.nil?

  [found.database.to_s, found.instance_variable_get(:@url).to_s]
end

def expect_suffixed(label, database)
  raise "#{label} stayed #{database.inspect}" unless database.end_with?(SUFFIX)
  raise "#{label} is not UTF-8: #{database.inspect}" unless database.valid_encoding?
end

def expect_same(label, database, original)
  raise "#{label} changed #{original.inspect} -> #{database.inspect}" unless database == original
end

trace_before = ObjectSpace.each_object(TracePoint).select(&:enabled?).map(&:object_id)
raise "the hook left #{trace_before.size} TracePoint(s) enabled" unless trace_before.empty?

hash = { "development" => { "adapter" => "postgresql", "database" => "app_dev" },
         "test" => { "adapter" => "postgresql", "database" => "app_test" } }
dev, = database_of(hash, "development")
tst, = database_of(hash, "test")
expect_suffixed("development", dev)
expect_suffixed("test", tst)
raise "development and test collided: #{dev}" if dev == tst

url_db, url = database_of({ "development" => "postgresql://localhost/app_dev" }, "development")
expect_suffixed("string url", url_db)
raise "string url left @url as #{url}" unless url.include?(url_db)

empty_db, empty_url = database_of(
  { "development" => { "adapter" => "postgresql", "url" => "postgresql://localhost", "database" => "app_dev" } },
  "development"
)
expect_suffixed("empty-path url", empty_db)
raise "empty-path url gained a database path: #{empty_url}" if empty_url.include?("app_dev#{SUFFIX}")

false_db, = database_of(
  { "development" => { "adapter" => "postgresql", "url" => "postgresql://localhost/app_dev", "replica" => "false" } },
  "development"
)
expect_suffixed('replica "false"', false_db)

remote, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "db.example", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_same("remote replica", remote, "app_dev")

local, = database_of(
  { "development" => { "adapter" => "postgresql", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_suffixed("local replica", local)

tasks, = database_of(
  { "development" => { "adapter" => "postgresql", "database" => "app_dev", "database_tasks" => false } },
  "development"
)
expect_same("database_tasks false", tasks, "app_dev")

sqlite, = database_of(
  { "development" => { "adapter" => "sqlite3", "database" => "db/development.sqlite3" } },
  "development"
)
expect_same("sqlite", sqlite, "db/development.sqlite3")

mixed_queue, = database_of(
  { "development" => { "queue" => { "adapter" => "sqlite3", "database" => "db/queue.sqlite3" },
                       "primary" => { "adapter" => "postgresql", "database" => "app_dev" } } },
  "development",
  "queue"
)
mixed_primary, = database_of(
  { "development" => { "queue" => { "adapter" => "sqlite3", "database" => "db/queue.sqlite3" },
                       "primary" => { "adapter" => "postgresql", "database" => "app_dev" } } },
  "development"
)
expect_same("sqlite beside postgres", mixed_queue, "db/queue.sqlite3")
expect_suffixed("postgres beside sqlite", mixed_primary)

prod, = database_of(
  { "production" => { "adapter" => "postgresql", "database" => "app_prod" } },
  "production"
)
expect_same("production", prod, "app_prod")

ENV["DATABASE_URL"] = "postgres://localhost/from_env"
from_env, from_url = database_of(
  { "development" => { "adapter" => "postgresql", "database" => "app_dev" } },
  "development"
)
ENV.delete("DATABASE_URL")
expect_suffixed("DATABASE_URL", from_env)
raise "DATABASE_URL @url was not suffixed: #{from_url}" unless from_url.include?(from_env)

# `.env.test` is loaded only when the test env boots. The development listing
# never sees it, so the hook has to suffix that URL itself.
ENV["RAILS_ENV"] = "test"
ENV["DATABASE_URL"] = "postgres://localhost/from_test_env"
test_db, test_url = database_of(
  { "test" => { "adapter" => "postgresql", "database" => "app_test" } },
  "test"
)
ENV.delete("DATABASE_URL")
ENV["RAILS_ENV"] = "development"
expect_suffixed("test DATABASE_URL", test_db)
raise "test DATABASE_URL @url was not suffixed: #{test_url}" unless test_url.include?(test_db)

ENV["QUEUE_DATABASE_URL"] = "postgres://localhost/from_queue"
queue_db, queue_url = database_of(
  { "development" => { "primary" => { "adapter" => "postgresql", "database" => "app_dev" },
                       "queue" => { "adapter" => "postgresql", "database" => "queue_dev" } } },
  "development",
  "queue"
)
ENV.delete("QUEUE_DATABASE_URL")
expect_suffixed("QUEUE_DATABASE_URL", queue_db)
raise "named url was not suffixed: #{queue_url}" unless queue_url.include?(queue_db)

long = "_" + ("s" * 55)
ENV["WORKON_RAILS_DB_SUFFIX"] = long
pair = configs_for(
  { "development" => { "primary" => { "adapter" => "postgresql", "database" => "primary_development" },
                       "analytics" => { "adapter" => "postgresql", "database" => "analytics_development" } } }
)
dev_names = pair.select { |config| config.env_name == "development" }.map { |config| config.database.to_s }
raise "long suffix collided: #{dev_names.inspect}" unless dev_names.uniq.size == dev_names.size
dev_names.each do |name|
  raise "long suffix blew the postgres cap: #{name.bytesize}" if name.bytesize > 63
  raise "long suffix is not UTF-8" unless name.valid_encoding?
end

mysql = configs_for(
  { "development" => { "primary" => { "adapter" => "mysql2", "database" => "m" * 60 },
                       "other" => { "adapter" => "trilogy", "database" => "t" * 60 } } }
)
mysql_names = mysql.select { |config| config.env_name == "development" }.map { |config| config.database.to_s }
raise "mysql names collided: #{mysql_names.inspect}" unless mysql_names.uniq.size == mysql_names.size
mysql_names.each do |name|
  raise "mysql name is #{name.bytesize} bytes" if name.bytesize > 64
  raise "mysql name dropped the suffix" unless name.end_with?(long)
end

utf = ("café" * 20) + "xx"
ENV["WORKON_RAILS_DB_SUFFIX"] = SUFFIX
utf_db, = database_of({ "development" => { "adapter" => "postgresql", "database" => utf } }, "development")
raise "utf-8 name is invalid: #{utf_db.inspect}" unless utf_db.valid_encoding?
raise "utf-8 name is #{utf_db.bytesize} bytes" if utf_db.bytesize > 63
expect_suffixed("utf-8", utf_db)

ENV["WORKON_RAILS_DB_SUFFIX"] = "_"
bare, = database_of({ "development" => { "adapter" => "postgresql", "database" => "app_dev" } }, "development")
expect_same("bare underscore", bare, "app_dev")

ENV["WORKON_RAILS_DB_SUFFIX"] = SUFFIX

socket_host, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "/var/run/postgresql", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_suffixed("socket directory host", socket_host)

service_host, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "postgres", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_suffixed("single-label host", service_host)

query_host, query_url = database_of(
  { "development" => "postgresql:///app_dev?host=/var/run/postgresql&replica=true" },
  "development"
)
expect_suffixed("url socket host", query_host)
raise "url socket host left @url as #{query_url}" unless query_url.include?(query_host)

false_hash, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "postgres", "database" => "app_dev", "replica" => "false" } },
  "development"
)
expect_suffixed('hash replica "false"', false_hash)

false_remote, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "db.example", "database" => "app_dev", "replica" => "false" } },
  "development"
)
expect_suffixed('hash replica "false" on a dotted host', false_remote)

ENV["DATABASE_URL"] = "postgres://localhost/from_env"
sqlite_env, sqlite_env_url = database_of(
  { "development" => { "adapter" => "sqlite3", "database" => "storage/development.sqlite3" } },
  "development"
)
ENV.delete("DATABASE_URL")
expect_suffixed("sqlite file plus DATABASE_URL", sqlite_env)
raise "sqlite DATABASE_URL @url was not suffixed: #{sqlite_env_url}" unless sqlite_env_url.include?(sqlite_env)

def owned(hash)
  WorkonRailsDbSuffix.owned_rows(ActiveRecord::DatabaseConfigurations.new(hash))
end

local_rows = owned({ "development" => { "primary" => { "adapter" => "postgresql", "database" => "app_dev", "replica" => true } } })
raise "local replica hidden from the listing: #{local_rows.inspect}" unless local_rows.any? { |row| row[2].end_with?(SUFFIX) }

socket_rows = owned(
  { "development" => { "primary" => { "adapter" => "postgresql", "host" => "/var/run/postgresql", "database" => "app_dev", "replica" => true } } }
)
raise "socket-directory replica hidden from the listing: #{socket_rows.inspect}" unless socket_rows.any? { |row| row[2].end_with?(SUFFIX) }

false_rows = owned(
  { "development" => { "primary" => { "adapter" => "postgresql", "host" => "db.example", "database" => "app_dev", "replica" => "false" } } }
)
raise "replica \"false\" hidden from the listing: #{false_rows.inspect}" unless false_rows.any? { |row| row[2].end_with?(SUFFIX) }

remote_rows = owned(
  { "development" => { "primary" => { "adapter" => "postgresql", "host" => "db.example", "database" => "app_dev", "replica" => true } } }
)
raise "remote replica was listed: #{remote_rows.inspect}" unless remote_rows.empty?

tasks_rows = owned(
  { "development" => { "primary" => { "adapter" => "postgresql", "database" => "app_dev", "database_tasks" => false } } }
)
raise "database_tasks false was listed: #{tasks_rows.inspect}" unless tasks_rows.empty?

trace_after = ObjectSpace.each_object(TracePoint).select(&:enabled?)
extra = trace_after.reject { |point| trace_before.include?(point.object_id) }
raise "the hook enabled #{extra.size} TracePoint(s)" unless extra.empty?

puts "OK"
