"""The dashboard: a Flask service that reads the JSON API over HTTP.

Deliberately separate from ``api``. The API is a JSON surface that a client can
point at without dragging a user interface along, and the dashboard is a viewer
that must keep working — and keep *explaining* — when the API is down. Two
processes, two failure domains, one HTTP contract between them.
"""

from dashboard.app import create_app

__all__ = ["create_app"]
