from django.db import connection
from django.test import TestCase


class SmokeTest(TestCase):
    def test_hits_the_database(self):
        with connection.cursor() as cursor:
            cursor.execute("SELECT 1")
            self.assertEqual(cursor.fetchone()[0], 1)
