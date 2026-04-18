// Driver that exercises the other modules. Calls are wrapped in `main()`
// so ASD's call-graph extractor produces cross-module edges like
// `driver.main -> logger.writeLog` and `driver.main -> payments.chargeCard`.

import * as logger from "./logger";
import * as payments from "./payments";
import { hello } from "./greetings";

export function main(): void {
    // Drive logger so the tracer sees fs+time effects.
    logger.writeLog("/tmp/asd-ts-trace-demo.log", "hello");
    // Drive a pure call (no effects).
    hello("world");
    // Cross-module via namespace import -> payments.chargeCard.
    try {
        payments.chargeCard("u1", 100.0);
    } catch {
        // FakeDB may or may not accept the call; we just want the edge.
    }
}
