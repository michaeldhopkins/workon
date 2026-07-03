# Workspace provisioners

Status: proposed (for review)
Target version: 0.21.0

## Goal

Generalize per-workspace setup beyond the single hard-coded Rails path into a
modular set of **provisioners**: workon detects which kinds of project live in a
repo (there can be several) and runs the setup each needs — but only the setup
that copying gitignored files *can't* provide. Detectors are all built in; no
project-provided scripts run (see Security).

Non-goal: re-installing dependencies. We copy `node_modules`, `vendor/bundle`,
`_build`, `deps`, `.venv`, etc. via `clonefile`, so `bundle install` / `npm
install` / `mix deps.get` are unnecessary and stay out. A provisioner exists only
for work copying leaves undone.

## What copying can't do

1. **Isolated test databases.** A shared dev DB can't be copied into a fresh
   per-workspace test DB; and even frameworks that create their *own* test DB
   derive its name from config, so two workspaces collide unless each is handed a
   distinct DB identity.
2. **Artifacts with absolute paths baked in.** These break when the worktree
   lands at `~/.worktrees/<project>-<ws_id>`. Two sub-cases, both Python:
   - a copied **`.venv`** — `bin/activate` and every console-script shebang
     hardcode the old venv path;
   - **editable installs** (`pip install -e .`) — `.pth` / `__editable__…finder.py`
     / `*.egg-link` in site-packages point at the old *source* dir (which workon
     also moved). Distinct from the venv-path rewrite.

Everything else — Go module cache, Rust `target`, Node `node_modules`, PHP
`vendor`, Elixir `_build`/`deps` — is relocatable, so those need **no**
provisioner. That's a property of the scope, not a gap.

## Two lifecycle modes (the core of the variance)

Research across ecosystems shows the *only* structural axis that matters is who
owns the test-DB lifecycle:

- **Workon-managed.** workon creates the DB, injects its name, runs the
  schema/migrate command, records it for teardown, and drops it. — Rails, Prisma
  (migrations), Alembic-on-Postgres, Phoenix/Ecto, EF Core against a real server.
- **Framework-managed (stay out of the way).** The test runner creates *and*
  drops its own DB every run (or runs entirely in-memory). workon creates
  nothing and records no resource — it only injects a **per-workspace DB
  identity** so the runner's auto-named DB doesn't collide with a sibling
  workspace's. — Django's default runner (`test_<NAME>`), Laravel sqlite/`:memory:`
  + `RefreshDatabase`, EF `InMemory`/Testcontainers, any sqlite-in-memory.

A provisioner picks its mode by **inspecting config**, not just file presence
(sqlite/in-memory ⇒ framework-managed / no-op). Returning an empty `Setup` is a
correct, intentional outcome — not a failure.

## Architecture

Mirrors the `Vcs` trait + backend registry.

```rust
pub trait Provisioner: Send + Sync {
    fn name(&self) -> &'static str;

    /// Detect from the (already-copied) worktree. May read a config file to
    /// decide — e.g. a sqlite datasource means "no server DB to manage".
    fn detect(&self, ws_dir: &Path) -> bool;

    /// Do the irreducible setup. Returns resources to tear down and env to
    /// inject into the workspace session. Empty `Setup` = intentional no-op.
    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup>;
}

pub struct ProvisionCtx<'a> {
    pub project_dir: &'a Path,   // the source repo — the old path venv/editable repair rewrites
    pub project_name: &'a str,
    pub ws_id: &'a str,
    pub ws_dir: &'a Path,
    pub mise_vars: &'a HashMap<String, String>,
}

pub struct Setup {
    pub resources: Vec<Resource>,       // teardown undoes these (empty when framework-managed)
    pub env: Vec<(String, String)>,     // injected into the workspace session (see Env delivery)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    PostgresDb { name: String },
    MysqlDb { name: String },
    // sqlite needs no entry: the file lives in the worktree; `rm -rf` takes it.
}
```

**Resource-before-risk rule.** A provisioner must push a droppable resource into
`Setup.resources` **the moment it creates it**, before any step that can fail
(migrate, schema load). On a later failure, return `Ok(Setup { .. })` with what
was created plus an `eprintln!` warning — never `Err` *after* creating something
droppable, or teardown won't know to drop it and the DB leaks. `Err` means
"created nothing."

**Shared DB machinery** — a `db` module so each provisioner doesn't re-roll it:

```rust
pub enum DbEngine { Postgres, Mysql, Sqlite }
impl DbEngine {
    fn create(&self, name: &str) -> Result<()>;   // createdb / mysqladmin create / (sqlite: noop)
    fn drop(&self, name: &str);                    // dropdb / mysqladmin drop / (sqlite: rm file)
    fn url(&self, name: &str) -> String;           // connection URL for the injected env var
}
/// 63-byte-safe, collision-free: keep the random ws_id, truncate/​hash human parts.
fn test_db_name(project_name: &str, ws_id: &str) -> String;
```

The engine is read from the framework's config (adapter in `database.yml`,
`provider` in `schema.prisma`, provider package in `*.csproj`, …), so a
sqlite/in-memory project is recognized and left alone.

### Env delivery (the linchpin)

An injected `DATABASE_URL` / `MIX_TEST_PARTITION` only isolates the DB if it
reaches the user's `rails test` / `mix test` **in the workspace session**, not
just a file. So:

- `Setup.env` is persisted in `.workon.json` (`env: { K: V }`) and **merged into
  the workspace session environment** — alongside mise env — at both `launch` and
  `attach`. `attach` re-reads it from `.workon.json`, so a fresh-process attach
  still gets the isolation. This requires `session::launch`/`attach` to take the
  merged env (a small change to today's mise-only env).
- `.env.test.local` (dotenv) is *also* written for the subset frameworks
  auto-load from dotenv (Rails+dotenv, Laravel, Prisma, Node) — this preserves
  today's behavior — but the session env is the source of truth. It stays in
  `GENERATED_FILES` + VCS-ignored, as now.

### Registry, ordering, conflicts

- `fn provisioners() -> Vec<Box<dyn Provisioner>>`, ordered. **`PythonVenv` repair
  runs first**, before Django/Alembic — they need a working interpreter.
- `provision` runs every provisioner whose `detect` fires, collecting resources +
  env.
- **Env-key collision** (a polyglot repo where Rails and Prisma both want
  `DATABASE_URL`): v1 is last-writer-wins with a warning naming the key and the
  two provisioners. Directory-scoped env (a Prisma app in `frontend/` reading its
  own `.env`) is future work; most polyglot repos already separate by directory.

## Per-ecosystem catalog

Each row: the signal that detects it, the DB engine source, the lifecycle mode,
the isolation hook (env), the prepare command, and teardown. Gotchas below the
table are load-bearing — they came out of the research and are the "fluency."

| Ecosystem | Detect | Mode | Isolation hook | Prepare | Teardown |
|---|---|---|---|---|---|
| **Rails** | `config/database.yml` (+ `bin/rails`) | workon-managed | `DATABASE_URL` + `RAILS_ENV=test` | `createdb`, then `RAILS_ENV=test bin/rails db:schema:load` | `dropdb <name>` |
| **Django** | `manage.py` | framework-managed | `DATABASE_URL` **iff** settings use `dj_database_url`; else none | none (runner builds `test_<NAME>`) | none |
| **Alembic** | `alembic.ini`/`[tool.alembic]` + `versions/` | workon-managed *if* Postgres | `DATABASE_URL` **iff** `env.py` reads it | `createdb`, `alembic upgrade head` | `dropdb <name>` |
| **Phoenix/Ecto** | `mix.exs` dep on `:ecto_sql` | workon-managed | `MIX_TEST_PARTITION` (built-in) | `MIX_ENV=test mix ecto.create && ecto.migrate` | `dropdb <app>_test<part>` |
| **Laravel** | `artisan` + `laravel/framework` | framework-managed if sqlite/`:memory:`; workon-managed if real DB | `.env.testing` `DB_DATABASE` | real DB: `createdb` (RefreshDatabase migrates) | `dropdb` or none |
| **Prisma** | `prisma/schema.prisma` | workon-managed unless `sqlite` | `DATABASE_URL` (read the actual `env()` name) | `migrate deploy` (or `db push` if no `migrations/`), then `prisma generate` | `DROP DATABASE` |
| **EF Core** | `*.csproj` w/ EF provider pkg + `Migrations/` | workon-managed unless InMemory/sqlite-mem/Testcontainers | `ConnectionStrings__<Name>` or `--connection` | `dotnet ef database update` | `dotnet ef database drop --force` |
| **Python venv** | `.venv/pyvenv.cfg` | (path repair) | — | rewrite paths / re-link editable installs | none |

Rails **`db:test:prepare` does not create the DB** (it purges + loads schema),
so we keep `createdb` + `db:schema:load`. Two traps: a `url:` key in
`database.yml` makes Rails ignore `DATABASE_URL`; and `DATABASE_URL` overrides
even under `RAILS_ENV=test`, so workon must point it *at the isolated test DB*
(a stray one would let test tasks purge dev/prod). `config/master.key` is
gitignored → copied → normally fine.

Django's default `manage.py test` runner creates and drops `test_<NAME>` itself,
so workon pre-creates nothing; isolation is only handing each workspace a
distinct `NAME` — and that's *only* injectable via `DATABASE_URL` when the
project uses `dj_database_url`. If it doesn't, workon can't cleanly isolate
without editing settings, so it does nothing and logs that (documented
limitation, not a silent gap). `pytest-django --reuse-db` leaves a persistent
`test_<NAME>` a future "reuse" mode could pre-migrate; out of scope for v1.

Alembic reads no env var by default (`-x` doesn't set the URL); workon can only
inject a DB name when `env.py` reads `DATABASE_URL`. Postgres DBs must be created
first (`alembic upgrade head` does DDL *in* a DB, doesn't create it). Many
Flask/FastAPI suites are sqlite-in-memory → no-op.

Phoenix's generated config names the test DB `#{app}_test#{MIX_TEST_PARTITION}`,
so setting `MIX_TEST_PARTITION` per workspace isolates it with zero file edits —
the cleanest hook of any ecosystem. `mix test` self-provisions via its generated
alias, but workon still runs `ecto.create && ecto.migrate` so the DB exists at
provision time and records the name for teardown. Detect on `:ecto_sql` (a
`--no-ecto` Phoenix app has no DB).

Laravel's default is sqlite `:memory:` + `RefreshDatabase` (migrates itself) →
**no-op**. Only when a real MySQL/PG test DB is configured does workon create it;
`RefreshDatabase` still migrates. `APP_KEY` rides in via the copied `.env`.

Prisma: read the real env-var name from the `datasource` (Prisma ≤6) or
`prisma.config.ts` (Prisma 7+, where `--url`/`--schema` were removed).
`migrate deploy` (has `migrations/`) or `db push --skip-generate` (no migrations)
both create the DB if absent; neither needs a shadow DB. Always run
`prisma generate` after — the copied client can be stale or built for a different
platform target. `sqlite` provider ⇒ no-op.

EF Core: detect the provider package to get the engine; `InMemory`, a
`:memory:` sqlite, or a `Testcontainers*` fixture ⇒ **stay out of the way**; a
real provider (+ optionally `Respawn`) ⇒ manage it. `dotnet ef database update`
creates + migrates but needs the `dotnet-ef` tool (prefer a `.config/
dotnet-tools.json` local tool, else global) and rebuilds by default (`--no-build`
to trust copied `bin/`). Override the connection via `ConnectionStrings__<Name>`
(read the real key) or the `--connection` flag.

**Python venv** repair, in order:
1. Rewrite the old **venv** path → `ws_dir/.venv` in `bin/activate*` and every
   `bin/*` shebang. **Leave `pyvenv.cfg` `home=` and the `bin/python` symlink
   alone** — they point at the (unmoved) system Python.
2. Editable installs: `.pth` / `__editable__…finder.py` / `*.egg-link` point at
   the old **source** dir (`project_dir`). The `finder.py` `MAPPING` dict is hard
   to rewrite reliably, so the robust move is to **re-link** from the new source —
   `uv pip install -e . --no-deps` (or `pip … --no-deps`) run in `ws_dir` — which
   regenerates the artifacts against the new path without touching dependencies
   or the network. Fall back to best-effort path rewrite + warn if no installer
   is present.

Explicitly **no provisioner** (copy suffices): Go, Rust, generic Node/Vite/Next
without Prisma, plain Java/Gradle.

## `.workon.json` migration

`created_db: Option<String>` → `resources: Vec<Resource>` plus an `env` map:

```json
{
  "base": "a1b2c3…",
  "config": null,
  "name": "fix bug",
  "resources": [ { "type": "postgres_db", "name": "mbc_ws_abc123_test" } ],
  "env": { "DATABASE_URL": "postgres://localhost/mbc_ws_abc123_test", "RAILS_ENV": "test" }
}
```

Back-compat on load: a legacy `created_db` with no `resources` maps to
`[PostgresDb { name: created_db }]`. All new fields are `#[serde(default)]`.

## End-to-end testing

Non-interactive `create`/`destroy` + the injectable `provision_in` seam make
provisioners directly testable: provision into a temp worktrees root, assert
external state, tear down, assert it's gone — no zellij, no real `~/.worktrees`,
no real `$HOME`.

### Tier 1 — detection tests (always run, no toolchain)

Per provisioner: a fixture that should trigger it → `detect()` true; a different
fixture → false. Include the **mode-selecting** cases: a sqlite `schema.prisma`
and an EF `InMemory` csproj must detect-and-skip (assert an *empty* `Setup`), a
real-provider one must not. Pure file/config inspection, deterministic.

### Tier 2 — provisioner contract harness (gated on toolchain)

One parameterized cycle per provisioner, **skipped** (not failed) when its
toolchain is absent — like the jj tests gate on `jj_available()`:

```
skip unless binary_available(required tools)
1. materialize tests/fixtures/<type>/ as a git repo in a tempdir
2. provision_in(temp_worktrees, fixture, …)              // HOME-isolated
3. assert:
   - workon-managed: the Resource is in Setup and .workon.json; the DB exists AND
     its schema is present (connect + query a migrated table); env carries the
     isolation var
   - framework-managed: Setup is empty of resources; env carries the identity var
   - venv: a console script in .venv/bin runs from the new path; an editable
     import resolves against ws_dir, not project_dir
4. teardown(SaveMode::NoSave)
5. assert the DB is dropped (connect → absent)
```

**Fixture dependencies (test-only, not production).** In production the deps are
copied. Fixtures are tiny synthetic repos with no deps, so the CI job
system-installs the framework (`gem install rails`, `npm i -g prisma`,
`pip install django alembic`) and the fixture uses it. This is the real
per-ecosystem test-maintenance cost and the reason each stack needs its own CI
job, not just a Postgres service.

**Maintenance shape:** add a provisioner = implement trait + drop a fixture under
`tests/fixtures/<type>/` + register it; the harness covers it. One cycle, N
fixtures.

**CI:** E2E cycles run as **separate jobs, one per ecosystem, in parallel**, each
with a Postgres `services:` container + its toolchain — wall clock is the slowest
job, not the sum:

- **Ruby/Rails** job → Rails cycle
- **Node** job → Prisma cycle
- **Python** job → Django, Alembic, and Python-venv cycles

Elixir/PHP/.NET run **detection** in the main job and their full cycle only where
the toolchain exists (gated skip); add a job per stack later. Each gated skip is
`log()`ged so a green run never overstates coverage.

**Isolation & safety:** every E2E test uses a temp `$HOME` + temp worktrees root;
DB names carry the random `ws_id`; teardown drops them and a test-scope guard
drops any leaked DB on failure.

## Security

No repo-provided setup scripts. Every provisioner is code we ship, running
workon's own logic against detected files — no new code-execution surface versus
today. (A future opt-in `.workon/setup` hook would ride the 0.18.0 config trust
gate; see Out of scope.)

## Out of scope (future)

- **Reuse mode**: pre-create + migrate a persistent DB for `pytest-django
  --reuse-db` / Rails `--keepdb`.
- **Generic SQL migrations** (dbmate/goose/flyway): "run only if no framework
  matched" isn't expressible with independent `detect()`; needs the loop to skip
  it when an earlier provisioner already produced a DB resource.
- **Parallel test databases** (`..._test-0/-1/…`, Rails `parallelize`, Laravel
  `--parallel`, pytest-xdist): provisioners make the single test DB only; the
  single-process workaround stays documented.
- **Directory-scoped env** for polyglot repos; **per-workspace service isolation**
  (docker-compose, Redis) with allocated ports.
- **`.workon/setup`** trust-gated project hook for unrecognized stacks.
- A scheduled CI matrix installing every toolchain for the full E2E set.

## Sequencing

1. `feat` — `Provisioner` trait, `Resource`, `DbEngine`, registry, and env
   delivery (session-env merge + `.workon.json` `env`/`resources`); extract
   today's Rails path into the first provisioner; `.workon.json` migration with
   back-compat. No behavior change for Rails users. Tier-1 + Rails Tier-2 (CI has
   pg + Rails).
2. `feat` — Python venv repair incl. editable-install re-link (fixes the current
   latent broken-venv bug) + tests.
3. `feat` — remaining provisioners, one per commit, each with fixture + detection
   test (+ gated cycle): Prisma, Django, Alembic, Phoenix, Laravel, EF Core.
