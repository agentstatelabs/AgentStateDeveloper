"""Payment processing with simulated DB writes, logging, and network calls.

This module exercises several effect categories so ASD's effect inference
has something to find: io.db.read, io.db.write, io.net, log, and throw.
"""

import logging
import urllib.request

log = logging.getLogger(__name__)


class FakeDB:
    """In-memory stand-in for a real DB driver. Pattern-compatible only."""

    def execute(self, query: str, *params: object) -> list[tuple]:
        # Return an empty result set; real driver would hit the wire here.
        return []


# Module-level handle so call sites look like `db.execute(...)` -- this is
# the shape ASD's pattern matcher expects when inferring io.db.* effects.
db = FakeDB()


def charge_card(user_id: str, amount: float) -> str:
    """Charge ``amount`` to ``user_id``. Writes a row and logs the charge."""
    if amount <= 0 or amount > 10000:
        raise ValueError(f"invalid amount: {amount}")
    log.info("charging user=%s amount=%s", user_id, amount)
    db.execute("INSERT INTO charges (user_id, amount) VALUES (?, ?)", user_id, amount)
    return f"charge:{user_id}:{amount}"


def get_balance(user_id: str) -> float:
    """Return the current balance for ``user_id`` (DB read + log)."""
    log.debug("fetching balance for user=%s", user_id)
    rows = db.execute("SELECT balance FROM accounts WHERE user_id = ?", user_id)
    return float(rows[0][0]) if rows else 0.0


class Payment:
    """A single payment record, with a refund path that hits net + db + log."""

    def __init__(self, user_id: str, amount: float) -> None:
        self.user_id = user_id
        self.amount = amount

    def refund(self, reason: str) -> str:
        """Issue a refund: notify upstream processor, log, then persist."""
        log.info("refunding user=%s amount=%s reason=%s", self.user_id, self.amount, reason)
        urllib.request.urlopen("https://payments.example.com/refund")
        db.execute("INSERT INTO refunds (user_id, amount) VALUES (?, ?)", self.user_id, self.amount)
        return f"refund:{self.user_id}:{self.amount}"
