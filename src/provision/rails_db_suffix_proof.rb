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

# An app whose database.yml already ends the name with the workspace suffix,
# past Postgres's 63 bytes. A process outside the session never loads the hook,
# sends the long name, and the server cuts it to 63 bytes. The hook must name
# the database the same way, or workon creates one and the app opens another.
def server_cut(name)
  cut = +""
  name.each_char { |char| break if cut.bytesize + char.bytesize > 63; cut << char }
  cut
end

def opened_by(suffix, names, adapter = "postgresql")
  ENV["WORKON_RAILS_DB_SUFFIX"] = suffix
  hash = { "development" => names.to_h { |key, name| [key, { "adapter" => adapter, "database" => name }] } }
  configs_for(hash).select { |config| config.env_name == "development" }.to_h { |config| [config.name, config.database.to_s] }
end

per_checkout_suffix = "_ws_a4af64_quarterly_reports_summary"
per_checkout = "mylittleapp_development_mylittleapp#{per_checkout_suffix}"
raise "the fixture name is not past the limit" unless per_checkout.bytesize > 63

ENV["WORKON_RAILS_DB_NAMES"] = "server"
created = opened_by(per_checkout_suffix, "primary" => per_checkout).fetch("primary")
unless created == server_cut(per_checkout)
  raise "workon would create #{created.inspect} but the server opens #{server_cut(per_checkout).inspect}"
end

# Another workspace of the same app gets a database of its own.
other_suffix = "_ws_b1c2d3_quarterly_reports_summary"
other = opened_by(other_suffix, "primary" => "mylittleapp_development_mylittleapp#{other_suffix}").fetch("primary")
raise "two workspaces share #{created}" if other == created

# A UTF-8 name is cut on a character boundary, as the server cuts it.
accented = "café_development_cafe#{per_checkout_suffix}"
accented_db = opened_by(per_checkout_suffix, "primary" => accented).fetch("primary")
raise "accented name differs from the server's cut" unless accented_db == server_cut(accented) && accented_db.valid_encoding?

# A name whose cut would lose the workspace's identity keeps the tagged form,
# which no workspace shares and which still ends with the suffix.
heavy = ("x" * 70) + per_checkout_suffix
heavy_db = opened_by(per_checkout_suffix, "primary" => heavy).fetch("primary")
raise "an identity-less cut #{heavy_db.inspect}" unless heavy_db.end_with?(per_checkout_suffix) && heavy_db.bytesize <= 63

# A name that does not already end with the suffix is suffixed as before.
fixed = opened_by(per_checkout_suffix, "primary" => "mylittleapp_development").fetch("primary")
expect_same("fixed name", fixed, "mylittleapp_development#{per_checkout_suffix}")

# MySQL has no server-side cut (it refuses a name over 64 bytes), so it is untouched.
mysql_long = "m" * 40 + per_checkout_suffix
mysql_db = opened_by(per_checkout_suffix, { "primary" => mysql_long }, "mysql2").fetch("primary")
raise "mysql name is #{mysql_db.bytesize} bytes" if mysql_db.bytesize > 64

# A workspace created before the setting keeps the tagged form its databases have.
ENV.delete("WORKON_RAILS_DB_NAMES")
legacy = opened_by(per_checkout_suffix, "primary" => per_checkout).fetch("primary")
raise "a legacy workspace changed to #{legacy.inspect}" if legacy == server_cut(per_checkout) || !legacy.end_with?(per_checkout_suffix)

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

local_replica_forms = {
  "loopback 127.0.0.2" => "127.0.0.2",
  "ipv6 loopback" => "::1",
  "ipv6 expanded loopback" => "0:0:0:0:0:0:0:1",
  "ipv6 fully written loopback" => "0000:0000:0000:0000:0000:0000:0000:0001",
  "ipv6 bracketed loopback" => "[::1]",
  "ipv4-mapped loopback" => "::ffff:127.0.0.1",
  "localhost" => "localhost",
  "localhost with a trailing dot" => "localhost.",
  "single-label underscore" => "postgres_db",
  "comma-separated local hosts" => "localhost,127.0.0.1",
}
local_replica_forms.each do |label, host|
  database, = database_of(
    { "development" => { "adapter" => "postgresql", "host" => host, "database" => "app_dev", "replica" => true } },
    "development"
  )
  expect_suffixed(label, database)
end

docker_host, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "host.docker.internal", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_same("dotted host.docker.internal", docker_host, "app_dev")

# libpq tries every listed host and connects to hostaddr, so a replica that
# can land on this machine through any of them must be suffixed.
mixed_replica_forms = {
  "loopback then remote" => { "host" => "127.0.0.1,db.example" },
  "localhost then remote" => { "host" => "localhost,db.example" },
  "socket directory then remote" => { "host" => "/var/run/postgresql,db.example" },
  "leading empty host" => { "host" => ",db.example" },
  "trailing empty host" => { "host" => "db.example," },
  "remote host with loopback hostaddr" => { "host" => "db.example", "hostaddr" => "127.0.0.1" },
  "remote host with bracketed ipv6 loopback hostaddr" => { "host" => "db.example", "hostaddr" => "[::1]" },
  "remote host with remote then loopback hostaddr" => { "host" => "db.example,db2.example", "hostaddr" => "10.0.0.5,127.0.0.1" },
}
mixed_replica_forms.each do |label, extra|
  database, = database_of(
    { "development" => { "adapter" => "postgresql", "database" => "app_dev", "replica" => true }.merge(extra) },
    "development"
  )
  expect_suffixed(label, database)
end

remote_hostaddr, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "db.example", "hostaddr" => "10.0.0.5", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_same("remote host with remote hostaddr", remote_hostaddr, "app_dev")

remote_list, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "db.example,db2.example", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_same("remote host list", remote_list, "app_dev")

localhost_remote_addr, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "localhost", "hostaddr" => "10.0.0.5", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_same("localhost host with remote hostaddr", localhost_remote_addr, "app_dev")

spaced_addr, = database_of(
  { "development" => { "adapter" => "postgresql", "host" => "db.example", "hostaddr" => "10.0.0.5, 127.0.0.1", "database" => "app_dev", "replica" => true } },
  "development"
)
expect_suffixed("hostaddr list with a space", spaced_addr)

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

# An app whose own database is `postgres` with schema_search_path "public" is
# still an app database: unsuffixed, a workspace would share the main checkout's.
real_postgres, = database_of({ "development" => { "adapter" => "postgresql", "database" => "postgres", "schema_search_path" => "public" } }, "development")
expect_suffixed("app database named postgres", real_postgres)

# Rails creates and drops databases over a `postgres` connection it builds
# itself in its Postgres tasks (public_schema_config); that one stays as Rails
# wrote it, so the maintenance database exists to connect to.
require "active_record/tasks/database_tasks"
app = configs_for({ "development" => { "adapter" => "postgresql", "database" => "app_dev" } }).first
tasks = ActiveRecord::Tasks::PostgreSQLDatabaseTasks.new(app)
tasks.send(:establish_connection, tasks.send(:public_schema_config))
expect_same("maintenance connection", ActiveRecord::Base.connection_db_config.database, "postgres")

# Zeitwerk aliases Kernel#require after the hook loads; the hook must survive it.
begin
  require "zeitwerk"
  require "set"
rescue LoadError
  nil
end
