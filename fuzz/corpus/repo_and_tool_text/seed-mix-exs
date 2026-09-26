defmodule PhxFixture.MixProject do
  use Mix.Project

  def project do
    [
      app: :phx_fixture,
      version: "0.1.0",
      elixir: "~> 1.15",
      start_permanent: false,
      deps: deps()
    ]
  end

  # No `mod:` — we don't want `mix` to auto-start the Repo (it would try to
  # connect before `ecto.create` runs). ecto.create/migrate start it themselves.
  def application do
    [extra_applications: [:logger]]
  end

  defp deps do
    [
      {:ecto_sql, "~> 3.10"},
      {:postgrex, ">= 0.0.0"}
    ]
  end
end
