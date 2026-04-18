// Payment processing with a simulated DB, logging, and a throw path.
// Exercises several effect categories so ASD has something to find.

class FakeDB {
    // In-memory stand-in for a real DB driver. Pattern-compatible only.
    query(_sql: string, ..._params: unknown[]): unknown[] {
        return [];
    }
}

// Module-level handle so call sites look like `db.query(...)` -- the
// shape ASD's pattern matcher expects when inferring io.db.* effects.
const db = new FakeDB();

export function chargeCard(userId: string, amount: number): string {
    if (amount <= 0 || amount > 10000) {
        throw new Error(`invalid amount: ${amount}`);
    }
    console.log(`charging user=${userId} amount=${amount}`);
    db.query("INSERT INTO charges (user_id, amount) VALUES (?, ?)", userId, amount);
    return `charge:${userId}:${amount}`;
}

export function getBalance(userId: string): number {
    console.debug(`fetching balance for user=${userId}`);
    const rows = db.query("SELECT balance FROM accounts WHERE user_id = ?", userId);
    return rows.length > 0 ? Number((rows[0] as [number])[0]) : 0;
}

export class Payment {
    constructor(private userId: string, private amount: number) {}

    refund(reason: string): string {
        console.log(`refunding user=${this.userId} amount=${this.amount} reason=${reason}`);
        db.query("INSERT INTO refunds (user_id, amount) VALUES (?, ?)", this.userId, this.amount);
        return `refund:${this.userId}:${this.amount}`;
    }
}
