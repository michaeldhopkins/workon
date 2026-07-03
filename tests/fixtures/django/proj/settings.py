import os

import dj_database_url

# Reading the DB from DATABASE_URL (via dj-database-url) is what lets workon hand
# this project an isolated per-workspace DB; Django's test runner then creates
# `test_<NAME>` from it.
SECRET_KEY = "workon-fixture-not-secret"
INSTALLED_APPS = [
    "django.contrib.contenttypes",
    "django.contrib.auth",
    "app",
]
DATABASES = {
    "default": dj_database_url.config(
        default=os.environ.get("DATABASE_URL", "postgres://localhost/placeholder"),
    ),
}
USE_TZ = True
