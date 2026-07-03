import sqlalchemy as sa

from alembic import op

revision = "0001"
down_revision = None
branch_labels = None
depends_on = None


def upgrade():
    op.create_table(
        "widgets",
        sa.Column("id", sa.Integer, primary_key=True),
        sa.Column("name", sa.String(255)),
    )


def downgrade():
    op.drop_table("widgets")
