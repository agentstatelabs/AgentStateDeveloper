// File-backed log with an env-var knob. Exercises fs + time + env effects.

import * as fs from "fs";

export function writeLog(path: string, msg: string): void {
    const ts = Date.now();
    fs.writeFileSync(path, `${ts} ${msg}\n`, { flag: "a" });
}

export function readLog(path: string): string[] {
    const data = fs.readFileSync(path, "utf8");
    return data.split("\n");
}

export function rotateIfBig(path: string, maxBytes: number = 0): boolean {
    if (maxBytes <= 0) {
        maxBytes = Number(process.env.LOG_MAX_BYTES ?? "1048576");
    }
    const size = fs.statSync(path).size;
    if (size > maxBytes) {
        fs.renameSync(path, `${path}.1`);
        return true;
    }
    return false;
}
