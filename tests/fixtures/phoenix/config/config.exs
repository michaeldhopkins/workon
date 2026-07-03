import Config

# Mirror a generated Phoenix test config: the DB name carries MIX_TEST_PARTITION
# so each workspace gets its own. Connection comes from the PG* client env (a
# socket-path PGHOST falls back to localhost, as postgrex needs TCP).
pghost = System.get_env("PGHOST")

hostname =
  if pghost in [nil, ""] or String.starts_with?(pghost || "", "/"),
    do: "localhost",
    else: pghost

config :phx_fixture, ecto_repos: [PhxFixture.Repo]

config :phx_fixture, PhxFixture.Repo,
  database: "phx_fixture_test#{System.get_env("MIX_TEST_PARTITION")}",
  username: System.get_env("PGUSER") || System.get_env("USER") || "postgres",
  password: System.get_env("PGPASSWORD") || "",
  hostname: hostname,
  port: String.to_integer(System.get_env("PGPORT") || "5432")
