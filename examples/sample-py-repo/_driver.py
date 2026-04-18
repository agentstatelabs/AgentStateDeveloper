"""Driver script that exercises the other sample modules.

Calls are wrapped in ``main()`` so ASD's call-graph extractor produces
`_driver.main → logger.write_log`, `_driver.main → payments.charge_card`,
etc. Module-scope calls aren't walked; M5 extracts call edges only from
inside function/method bodies.
"""

import payments, logger
import greetings
from payments import charge_card


def main() -> None:
    # Drive logger so the tracer sees fs+time effects
    logger.write_log("/tmp/asd-trace-demo.log", "hello")
    # Drive a pure call (no effects)
    greetings.hello("world")
    # Cross-module via `from X import Y` — should resolve to payments.charge_card
    try:
        charge_card("u1", 100.0)
    except Exception:
        # FakeDB may or may not accept the call; we just want the edge.
        pass


if __name__ == "__main__":
    main()
