import os

from sqlalchemy import engine_from_config, pool

from alembic import context

config = context.config

# Honor an injected DATABASE_URL — how a real project makes Alembic target the
# per-workspace test DB workon created.
url = os.environ.get("DATABASE_URL")
if url:
    config.set_main_option("sqlalchemy.url", url)

connectable = engine_from_config(
    config.get_section(config.config_ini_section, {}),
    prefix="sqlalchemy.",
    poolclass=pool.NullPool,
)

with connectable.connect() as connection:
    context.configure(connection=connection, target_metadata=None)
    with context.begin_transaction():
        context.run_migrations()
