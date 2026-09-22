"""The search page.

    uv run python -m searchui.app

A Google-shaped front end over the query API: one search box, a list of results,
and paging. It renders on the server, so a query is a URL that can be shared,
bookmarked and reloaded, and the page works with JavaScript switched off. The
script that does load only adds keyboard affordances on top.

Kept apart from ``dashboard`` because the two answer different questions. The
dashboard reports on the system to whoever runs it; this is the system. Sharing a
process would mean a query surge slowing the monitoring, and a dashboard crash
taking the search page with it.
"""

from __future__ import annotations
